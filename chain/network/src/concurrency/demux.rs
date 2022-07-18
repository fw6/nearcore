//! A rate-limited demultiplexer.
//! It can be useful for example, if you want to aggregate a bunch of
//! network requests produced by unrelated routines into a single
//! bulk message to rate limit the QPS.
//!
//! Example usage:
//! `
//! let d = Demux::new(RateLimit(10.,1));
//! ...
//! let res = d.call(arg,|inout| async {
//!   // Process all inout[i].arg together.
//!   // Send the results to inout[i].out.
//! }).await;
//! `
//! If d.call is called simultaneously multiple times,
//! the arguments `arg` will be collected and just one
//! of the provided handlers will be executed asynchronously
//! (other handlers will be dropped).
//!
use futures::future::BoxFuture;
use futures::FutureExt;
use near_network_primitives::time;
use std::future::Future;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// Boxed asynchronous function. In rust asynchronous functions
/// are just regular functions which return a Future.
/// This is a convenience alias to express a
pub type BoxAsyncFn<Arg, Res> = Box<dyn 'static + Send + FnOnce(Arg) -> BoxFuture<'static, Res>>;

/// AsyncFn trait represents asynchronous functions which can be boxed.
/// As a simplification (which makes the error messages more readable)
/// we require the argument and result types to be 'static + Send, which is
/// usually required anyway in practice due to Rust limitations.
pub trait AsyncFn<Arg: 'static + Send, Res: 'static + Send> {
    fn wrap(self) -> BoxAsyncFn<Arg, Res>;
}

impl<F, Arg, Res, Fut> AsyncFn<Arg, Res> for F
where
    F: 'static + Send + FnOnce(Arg) -> Fut,
    Fut: 'static + Send + Future<Output = Res>,
    Arg: 'static + Send,
    Res: 'static + Send,
{
    fn wrap(self) -> BoxAsyncFn<Arg, Res> {
        Box::new(move |a: Arg| self(a).boxed())
    }
}
/// Config of a rate limiter algorithm, which behaves like a semaphore
/// - with maximal capacity `burst`
/// - with a new ticket added automatically every 1/qps seconds (qps stands for "queries per
///   second")
/// In case of large load, semaphore will be empty most of the time,
/// letting through requests at frequency `qps`.
/// In case a number of requests come after a period of inactivity, semaphore will immediately
/// let through up to `burst` requests, before going into the previous mode.
#[derive(Copy, Clone)]
pub struct RateLimit {
    pub burst: u64,
    pub qps: f64,
}

/// A demux handler should be in practice of type [Arg;n] -> [Res;n] for arbitrary n.
/// We approximate that by a function Vec<Arg> -> Vec<Res>. If the sizes do not match,
/// demux will panic.
type Handler<Arg, Res> = BoxAsyncFn<Vec<Arg>, Vec<Res>>;

struct Call<Arg, Res> {
    arg: Arg,
    out: oneshot::Sender<Res>,
    handler: Handler<Arg, Res>,
}
type Stream<Arg, Res> = mpsc::UnboundedSender<Call<Arg, Res>>;

/// Rate limited demultiplexer.
/// The current implementation spawns a dedicated future with an infinite loop to
/// aggregate the requests, and every bulk of requests is handled also in a separate spawned
/// future. The drawback of this approach is that every `d.call()` call requires to specify a
/// handler (and with multiple call sites these handlers might be inconsistent).
/// Instead we could technically make the handler an argument of the new() call. Then however
/// we risk that the handler will (indirectly) store the reference to the demux, therefore creating
/// a reference loop. In such case we get a memory leak: the spawned demux-handling future will never be cleaned
/// up, because the channel will never be closed.
///
/// Alternatives:
/// - use a separate closing signal (a golang-like structured concurrency).
/// - get rid of the dedicated futures whatsoever and make one of the callers do the work:
///   callers may synchronize and select a leader to execute the handler. This will however make
///   the demux implementation way more complicated.
#[derive(Clone)]
pub struct Demux<Arg, Res>(Stream<Arg, Res>);

impl<Arg: 'static + Send, Res: 'static + Send> Demux<Arg, Res> {
    pub fn call(
        &self,
        arg: Arg,
        f: impl AsyncFn<Vec<Arg>, Vec<Res>>,
    ) -> impl std::future::Future<Output = Res> {
        let stream = self.0.clone();
        async move {
            let (send, recv) = oneshot::channel();
            // ok().unwrap(), because DemuxCall doesn't implement Debug.
            stream.send(Call { arg, out: send, handler: f.wrap() }).ok().unwrap();
            recv.await.unwrap()
        }
    }

    pub fn new(rl: RateLimit) -> Demux<Arg, Res> {
        let (send, mut recv): (Stream<Arg, Res>, _) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut calls = vec![];
            let mut closed = false;
            let mut tokens = rl.burst;
            let mut next_token = None;
            let interval = (time::Duration::SECOND / rl.qps).try_into().unwrap();
            while !calls.is_empty() || !closed {
                // Restarting the timer every time a new request comes could
                // cause a starvation, so we compute the next token arrival time
                // just once for each token.
                if tokens < rl.burst && next_token.is_none() {
                    next_token = Some(tokio::time::Instant::now() + interval);
                }
                tokio::select! {
                    // TODO(gprusak): implement sleep future support for FakeClock,
                    // so that we don't use tokio directly here.
                    _ = async {
                        // without async {} wrapper, next_token.unwrap() would be evaluated
                        // unconditionally.
                        tokio::time::sleep_until(next_token.unwrap()).await
                    }, if next_token.is_some() => {
                        tokens += 1;
                        next_token = None;
                    }
                    call = recv.recv(), if !closed => match call {
                        Some(call) => calls.push(call),
                        None => closed = true,
                    },
                }
                if !calls.is_empty() && tokens > 0 {
                    tokens -= 1;
                    // TODO(gprusak): as of now Demux (as a concurrency primitive) doesn't support
                    // cancellation. Once we add cancellation support, this task could accept a context sum:
                    // the sum is valid iff any context is valid.
                    let calls = std::mem::take(&mut calls);
                    tokio::spawn(async move {
                        let mut args = vec![];
                        let mut outs = vec![];
                        let mut handlers = vec![];
                        for call in calls {
                            args.push(call.arg);
                            outs.push(call.out);
                            handlers.push(call.handler);
                        }
                        // A fancy way of extracting first element and dropping everything else.
                        // Just calling (handler[0])(args).await, would require handlers to
                        // implement Sync for some reason.
                        let handler = handlers.into_iter().next().unwrap();
                        let res = (handler)(args).await;
                        if res.len() != outs.len() {
                            panic!(
                                "demux handler returned {} results, expected {}",
                                res.len(),
                                outs.len()
                            );
                        }
                        for (res, out) in res.into_iter().zip(outs.into_iter()) {
                            // If the caller is no longer interested in the result,
                            // the channel will be closed. Ignore that.
                            let _ = out.send(res);
                        }
                    });
                }
            }
        });
        Demux(send)
    }
}
