#![warn(missing_docs)]
//! Fallback: transport-neutral attempt control, which the router's routes
//! (`respan_gateway_plugin_routing::Dispatch`) and other plugins use:
//! sequential retries and fallback ([`first_success`],
//! [`first_success_keeping`], [`Backoff`]), a
//! staggered race ([`race`]), and each attempt's [`Role`] ([`Attempt`]).
//! The caller supplies a retry classifier; HTTP callers can use
//! `respan_gateway_http_faults::retryable`.
//!
//! Both helpers read each answer [`Until`] a stopping point, using a
//! plugin-supplied predicate for [`Until::content`]: they return a success
//! only once it got there, and never retry after that, so a stream that fails
//! later reaches the client cut short, not replayed. An attempt policy
//! routes do not cover is a few lines of `on_client`:
//!
//! ```ignore
//! let calls = ["openai", "anthropic"].map(|p| cx.call(p));
//! let backoff = Backoff::new(3, Duration::from_millis(200), Duration::from_secs(10));
//! first_success(calls, &backoff, Until::content(Output::is_content),
//!     respan_gateway_http_faults::retryable).await.map(Some)
//! ```

use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use respan_gateway_core::futures::future::BoxFuture;
use respan_gateway_core::futures::stream::FuturesUnordered;
use respan_gateway_core::futures::{FutureExt, StreamExt};
use respan_gateway_core::{Call, Fault, Response, RetryAfter};

/// How often to try one call, and how long to wait between tries.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    tries: u32,
    base: Duration,
    max: Duration,
}

impl Backoff {
    /// Up to `tries` tries per call, waiting `base`, doubled after each
    /// retry, at most `max`. An optional response-supplied delay is waited
    /// for as asked, or, if longer than `max`, not at all: the call is not retried.
    pub fn new(tries: u32, base: Duration, max: Duration) -> Self {
        Self { tries, base, max }
    }

    /// One try per call: no retries.
    pub fn once() -> Self {
        Self::new(1, Duration::ZERO, Duration::ZERO)
    }

    /// The most tries per call.
    pub fn tries(&self) -> u32 {
        self.tries
    }

    /// The wait before another try, after `tries` tries; `None` if the
    /// attempt budget is spent or the supplied delay exceeds `max`.
    fn wait(&self, tries: u32, delay: Option<Duration>) -> Option<Duration> {
        if tries >= self.tries {
            return None;
        }
        // Saturating multiplication on Duration, not on a u32 factor: at
        // attempt 34 and beyond a sub-nanosecond base must still double.
        // Never loop proportional to an untrusted attempt budget.
        let factor = 1u128
            .checked_shl(tries.saturating_sub(1))
            .unwrap_or(u128::MAX);
        let nanos = self
            .base
            .as_nanos()
            .saturating_mul(factor)
            .min(self.max.as_nanos());
        let backoff = Duration::new(
            (nanos / 1_000_000_000) as u64,
            (nanos % 1_000_000_000) as u32,
        );
        match delay {
            Some(asked) if asked > self.max => None,
            Some(asked) => Some(asked),
            None => Some(backoff),
        }
    }
}

/// Put on every call [`first_success`] and [`race`] send
/// (read as `px.extensions()`),
/// so provider-edge hooks can tell attempts apart.
#[derive(Debug, Clone)]
pub struct Attempt {
    index: u32,
    won: Arc<AtomicBool>,
    head: Arc<OnceLock<Duration>>,
}

/// What became of an attempt.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Its response is the one returned.
    Winner,
    /// Any other: failed, superseded, dropped, or not chosen yet.
    Loser,
}

impl Attempt {
    fn new(index: u32) -> Self {
        let won = Arc::new(AtomicBool::new(false));
        let head = Arc::default();
        Self { index, won, head }
    }

    /// How long its response took to open, once it did: a provider's time
    /// to its head, for a router that ranks providers by speed.
    pub fn head(&self) -> Option<Duration> {
        self.head.get().copied()
    }

    /// Its place among the helper's attempts, in the order sent, from 0:
    /// what tells a retry from the first try in `on_provider`.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// What became of it. Every attempt is a loser until the helper returns
    /// its response, promoted before that response is read, so its
    /// `on_provider_end` always sees the final role.
    pub fn role(&self) -> Role {
        if self.won.load(Ordering::SeqCst) {
            Role::Winner
        } else {
            Role::Loser
        }
    }
}

type ContentReader =
    Arc<dyn Fn(Response) -> BoxFuture<'static, (Response, Result<(), Fault>)> + Send + Sync>;

/// How far [`first_success`] and [`race`] read an answer before they judge
/// it: a success that fails before that point is retried, or loses the
/// race; one that fails after it is committed.
///
/// A response whose call carries live (streaming) inputs is not read by
/// [`Until::content`] or [`Until::End`], because holding it could deadlock
/// the peer: such an answer commits as at [`Until::Head`].
#[non_exhaustive]
#[derive(Clone)]
pub enum Until {
    /// The response opening, without reading events. A provider's refusal
    /// (such as a 429 or 503 answer) is a failure here, not a success.
    Head,
    /// A plugin's condition for committing to a response. Use [`Until::content`].
    Content(ContentReader),
    /// The entire response; only a clean completion wins.
    End,
}

impl Until {
    /// Wait for an event accepted by this plugin's content policy.
    pub fn content<T: Any + Send + Sync>(
        predicate: impl FnMut(&T) -> bool + Clone + Send + Sync + 'static,
    ) -> Self {
        Self::Content(Arc::new(move |response| {
            let predicate = predicate.clone();
            async move {
                let mut response = response;
                let held = response.buffer_until(predicate).await;
                (response, held)
            }
            .boxed()
        }))
    }
}

impl std::fmt::Debug for Until {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Head => "Head",
            Self::Content(_) => "Content",
            Self::End => "End",
        })
    }
}

/// Retain an opened response independently of the result of inspecting it.
/// An errored final candidate must still be replayable, with its call alive.
struct Answer {
    attempt: Attempt,
    /// The opened response and how its inspection went, or the failure to open.
    outcome: Result<(Response, Option<Fault>), Fault>,
}

impl Answer {
    fn opened(&self) -> bool {
        self.outcome.is_ok()
    }

    /// Why the attempt failed, if it did.
    fn fault(&self) -> Option<&Fault> {
        match &self.outcome {
            Ok((_, inspected)) => inspected.as_ref(),
            Err(fault) => Some(fault),
        }
    }
}

/// Send `call` as `attempt` and read it `until` a point.
async fn send(call: Call, attempt: Attempt, until: Until) -> Answer {
    let sent = Instant::now();
    let outcome = match call.with_extension(attempt.clone()).await {
        Ok(mut response) => {
            let _ = attempt.head.set(sent.elapsed());
            let inspected = match until {
                // A provider's refusal is an opened response, but not a success.
                Until::Head if response.is_refused() => response.until_end().await,
                Until::Head => Ok(()),
                Until::Content(read) => {
                    let (held, result) = read(response).await;
                    response = held;
                    result
                }
                Until::End => response.until_end().await,
            };
            Ok((response, inspected.err()))
        }
        Err(fault) => Err(fault),
    };
    Answer { attempt, outcome }
}

/// Promote even an errored opened response before it is replayed and its
/// provider end hooks settle. A pre-opening fault has no response to commit.
fn win(answer: Answer) -> Result<Response, Fault> {
    let (response, inspected) = answer.outcome?;
    // A failed inspection replays only a response that holds its own
    // failed end, which cannot pass for healthy output; a wrong
    // predicate type or a custom inspection failure on a still-healthy
    // response is returned as that fault.
    if let Some(fault) = inspected
        && !matches!(response.buffered_end(), Some(Err(_)))
    {
        return Err(fault);
    }
    answer.attempt.won.store(true, Ordering::SeqCst);
    Ok(response)
}

/// The last answer; with none, `upstream`.
fn last(answer: Option<Answer>) -> Result<Response, Fault> {
    win(answer.ok_or_else(|| Fault::upstream("no provider to try"))?)
}

/// Prefer an opened candidate over a fallback that never opened, and keep
/// the failure `replaces` says a later one does not replace. Replacing one
/// drops its response as a loser, settling its pending call.
fn keep(previous: &mut Option<Answer>, answer: Answer, replaces: impl Fn(&Fault, &Fault) -> bool) {
    let is_kept = previous.as_ref().is_some_and(|kept| {
        (kept.opened() && !answer.opened())
            || matches!(
                (answer.fault(), kept.fault()),
                (Some(later), Some(earlier)) if !replaces(later, earlier)
            )
    });
    if !is_kept {
        *previous = Some(answer);
    }
}

/// Each call in turn, retried when `should_retry` permits and `backoff`
/// allows, until one's response is a success read `until` its stopping point;
/// else the last answer, preferring a response over a fallback that could not
/// be made. The classifier is called for **every** failed attempt, including
/// failures before opening, while reading content, and at [`Until::End`]. Its
/// `response_opened` argument is true exactly when `call.await` returned
/// `Ok(Response)` before applying `until`; it does not mean the request was
/// sent, a refusal occurred, or bytes were delivered. Returning false skips
/// retries of that target but still permits the next fallback. Only opened
/// responses contribute their [`RetryAfter`] extension to the backoff wait. HTTP
/// callers can select `respan_gateway_http_faults::retryable`; other transports can
/// supply their own policy. Neither policy nor timing makes an input replayable.
pub async fn first_success(
    calls: impl IntoIterator<Item = Call>,
    backoff: &Backoff,
    until: Until,
    should_retry: impl Fn(&Fault, bool) -> bool,
) -> Result<Response, Fault> {
    first_success_keeping(calls, backoff, until, should_retry, |_, _| true).await
}

/// [`first_success`], where a later failure takes the place of the one kept
/// only if `replaces(later, kept)`, so that a failure which says more of the
/// request is not hidden by a vaguer one after it. A response is still
/// preferred over a fallback that could not be made.
pub async fn first_success_keeping(
    calls: impl IntoIterator<Item = Call>,
    backoff: &Backoff,
    until: Until,
    should_retry: impl Fn(&Fault, bool) -> bool,
    replaces: impl Fn(&Fault, &Fault) -> bool,
) -> Result<Response, Fault> {
    let (mut previous, mut index) = (None, 0u32);
    for call in calls {
        // Zero tries dispatches nothing, including no provider hooks or
        // classifier callbacks. The bounded range avoids u32 overflow.
        for tries in 1..=backoff.tries {
            let answer = send(call.clone(), Attempt::new(index), until.clone()).await;
            index = index.saturating_add(1);
            let wait = match answer.fault() {
                None => return win(answer),
                Some(f) if should_retry(f, answer.opened()) => {
                    let delay = if answer.opened() {
                        f.get::<RetryAfter>().map(|r| r.0)
                    } else {
                        None
                    };
                    backoff.wait(tries, delay)
                }
                _ => None,
            };
            let Some(wait) = wait else {
                keep(&mut previous, answer, &replaces);
                break;
            };
            // Do not hold a failed attempt's call through backoff: its end
            // hooks and reservations must settle before a retry of the same target.
            drop(answer);
            tokio::time::sleep(wait).await;
        }
    }
    last(previous)
}

/// Start each call `stagger` after the one before, or at once when every
/// call started has failed. The first success read `until` its stopping
/// point wins, with [`Until::End`] only if it ended cleanly; the others
/// are dropped, so they end `abandoned`. With none, the last answer.
pub async fn race(
    calls: impl IntoIterator<Item = Call>,
    stagger: Duration,
    until: Until,
) -> Result<Response, Fault> {
    // A century stands for "never hedge": `Duration::MAX` would overflow.
    let stagger = stagger.min(Duration::from_secs(100 * 365 * 24 * 3600));
    let mut waiting = calls.into_iter().peekable();
    let mut index = 0u32;
    let mut running = FuturesUnordered::<BoxFuture<'static, Answer>>::new();
    let mut previous = None;
    let next = tokio::time::sleep(Duration::ZERO);
    tokio::pin!(next);
    loop {
        tokio::select! {
            Some(answer) = running.next() => {
                if answer.fault().is_none() {
                    return win(answer);
                }
                keep(&mut previous, answer, |_, _| true);
                if running.is_empty() {
                    next.as_mut().reset(tokio::time::Instant::now());
                }
            }
            () = &mut next, if waiting.peek().is_some() => {
                if let Some(call) = waiting.next() {
                    running.push(send(call, Attempt::new(index), until.clone()).boxed());
                    index = index.saturating_add(1);
                }
                next.as_mut().reset(tokio::time::Instant::now() + stagger);
            }
            else => return last(previous),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Backoff;

    #[test]
    fn backoff_doubles_beyond_a_u32_multiplier_and_caps_extreme_budgets() {
        let base = Duration::from_nanos(1);
        let backoff = Backoff::new(u32::MAX, base, Duration::MAX);
        for index in 32..=35 {
            assert_eq!(
                backoff.wait(index, None),
                Some(Duration::from_nanos(1u64 << (index - 1)))
            );
        }
        // At 34 the multiplier no longer fits u32; the duration still doubles.
        assert_eq!(backoff.wait(33, None), Some(Duration::from_nanos(1 << 32)));
        assert_eq!(backoff.wait(34, None), Some(Duration::from_nanos(1 << 33)));
        assert_eq!(backoff.wait(u32::MAX - 1, None), Some(Duration::MAX));
        assert_eq!(backoff.wait(u32::MAX, None), None);
    }

    #[test]
    fn backoff_doubles_up_to_its_max_and_bounds_supplied_delays() {
        let backoff = Backoff::new(5, Duration::from_millis(400), Duration::from_secs(1));
        let waits: Vec<_> = (1..=5).map(|n| backoff.wait(n, None)).collect();
        let ms = |n| Some(Duration::from_millis(n));
        assert_eq!(waits, [ms(400), ms(800), ms(1000), ms(1000), None]);
        assert_eq!(backoff.wait(1, Some(Duration::from_secs(1))), ms(1000));
        assert_eq!(backoff.wait(1, Some(Duration::from_secs(2))), None);
        assert_eq!(backoff.wait(5, Some(Duration::ZERO)), None);
    }
}
