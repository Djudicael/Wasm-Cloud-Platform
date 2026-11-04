//! Atomic admission/idle retirement for one HTTP instance.
use hyper::body::{Body, Frame, SizeHint};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

#[derive(Debug)]
struct State {
    active: usize,
    last_completed: Instant,
    retiring: bool,
    http_serving: bool,
}

#[derive(Debug)]
pub struct RequestActivity {
    state: Mutex<State>,
}

impl Default for RequestActivity {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                active: 0,
                last_completed: Instant::now(),
                retiring: false,
                http_serving: false,
            }),
        }
    }
}

impl RequestActivity {
    pub fn enable_http_serving(&self) {
        self.state.lock().expect("HTTP activity lock").http_serving = true;
    }
    pub fn is_http_serving(&self) -> bool {
        // Unknown state must not fall through to legacy timestamp pruning.
        self.state
            .lock()
            .map(|state| state.http_serving)
            .unwrap_or(true)
    }
    /// Admission and idle retirement share this lock, with no await inside it.
    pub fn begin(self: &Arc<Self>) -> Option<RequestGuard> {
        let mut state = self.state.lock().ok()?;
        if state.retiring {
            return None;
        }
        state.active = state.active.checked_add(1)?;
        Some(RequestGuard(Arc::new(Lease {
            activity: self.clone(),
        })))
    }

    pub fn is_idle(&self, timeout: Duration) -> bool {
        self.state.lock().is_ok_and(|state| {
            !state.retiring && state.active == 0 && state.last_completed.elapsed() > timeout
        })
    }

    /// Claim retirement atomically. A successful claim permanently fences new
    /// admission; callers must immediately remove this instance from routing.
    pub fn try_retire_if_idle(&self, timeout: Duration) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state.retiring || state.active != 0 || state.last_completed.elapsed() <= timeout {
            return false;
        }
        state.retiring = true;
        true
    }

    /// Explicit operator/quota shutdown remains able to stop active work.
    pub fn retire(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.retiring = true;
        }
    }
}

#[derive(Debug)]
struct Lease {
    activity: Arc<RequestActivity>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Ok(mut state) = self.activity.state.lock() {
            state.active -= 1;
            state.last_completed = Instant::now();
        }
    }
}

/// Clones represent the same request; activity ends only after the final owner
/// (guest execution and response body) releases it.
#[derive(Clone, Debug)]
pub struct RequestGuard(#[allow(dead_code)] Arc<Lease>);

/// Retains activity after response headers until EOF, terminal error or Drop.
pub(crate) struct ActivityBody<B> {
    inner: Pin<Box<B>>,
    guard: Option<RequestGuard>,
}
impl<B> ActivityBody<B> {
    pub(crate) fn new(body: B, guard: Option<RequestGuard>) -> Self {
        Self {
            inner: Box::pin(body),
            guard,
        }
    }
}
impl<B: Body> Body for ActivityBody<B> {
    type Data = B::Data;
    type Error = B::Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let result = self.inner.as_mut().poll_frame(cx);
        if matches!(result, Poll::Ready(None | Some(Err(_))))
            || (result.is_ready() && self.inner.is_end_stream())
        {
            self.guard.take();
        }
        result
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Full};

    #[test]
    fn handler_and_unconsumed_body_both_protect_retirement() {
        let activity = Arc::new(RequestActivity::default());
        let handler = activity.begin().unwrap();
        let body = ActivityBody::new(
            Full::new(bytes::Bytes::from_static(b"response")),
            Some(handler.clone()),
        );
        assert!(!activity.try_retire_if_idle(Duration::ZERO));
        drop(handler);
        assert!(!activity.try_retire_if_idle(Duration::ZERO));
        drop(body);
        assert!(activity.try_retire_if_idle(Duration::ZERO));
        assert!(activity.begin().is_none());
    }

    #[tokio::test]
    async fn eof_and_cancel_release_activity() {
        let activity = Arc::new(RequestActivity::default());
        let body = ActivityBody::new(
            Full::new(bytes::Bytes::from_static(b"response")),
            activity.begin(),
        );
        assert_eq!(body.collect().await.unwrap().to_bytes(), "response");
        assert!(activity.is_idle(Duration::ZERO));
        let cloned = activity.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _guard = cloned.begin().unwrap();
            let _ = ready_tx.send(());
            std::future::pending::<()>().await;
        });
        ready_rx.await.unwrap();
        assert!(!activity.try_retire_if_idle(Duration::ZERO));
        task.abort();
        let _ = task.await;
        assert!(activity.try_retire_if_idle(Duration::ZERO));
    }

    #[test]
    fn admission_and_retirement_have_one_winner() {
        for _ in 0..64 {
            let activity = Arc::new(RequestActivity::default());
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let cloned = activity.clone();
            let gate = barrier.clone();
            let thread = std::thread::spawn(move || {
                gate.wait();
                cloned.begin()
            });
            barrier.wait();
            let retired = activity.try_retire_if_idle(Duration::ZERO);
            let guard = thread.join().unwrap();
            assert_ne!(retired, guard.is_some());
        }
    }
}
