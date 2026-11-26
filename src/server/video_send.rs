use std::future::Future;

pub async fn complete_video_send<F, T, E, N>(send: F, notify: N) -> Result<T, E>
where
    F: Future<Output = Result<T, E>>,
    N: FnOnce(),
{
    let value = send.await?;
    notify();
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        future::Future,
        pin::Pin,
        ptr,
        rc::Rc,
        task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
    };

    fn noop_waker() -> Waker {
        unsafe fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(ptr::null(), &VTABLE)
        }
        unsafe fn wake(_: *const ()) {}
        unsafe fn wake_by_ref(_: *const ()) {}
        unsafe fn drop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
        unsafe { Waker::from_raw(RawWaker::new(ptr::null(), &VTABLE)) }
    }

    struct GatedSend {
        ready: Rc<Cell<bool>>,
        fail: bool,
    }

    impl Future for GatedSend {
        type Output = Result<(), &'static str>;

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            if !self.ready.get() {
                return Poll::Pending;
            }
            if self.fail {
                Poll::Ready(Err("send failed"))
            } else {
                Poll::Ready(Ok(()))
            }
        }
    }

    #[test]
    fn non_ack_frame_is_fetched_only_after_send_completes() {
        let ready = Rc::new(Cell::new(false));
        let notified = Rc::new(Cell::new(false));
        let mut future = Box::pin(complete_video_send(
            GatedSend {
                ready: ready.clone(),
                fail: false,
            },
            {
                let notified = notified.clone();
                move || notified.set(true)
            },
        ));
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);

        assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
        assert!(!notified.get());
        ready.set(true);
        assert!(matches!(
            future.as_mut().poll(&mut context),
            Poll::Ready(Ok(()))
        ));
        assert!(notified.get());
    }

    #[test]
    fn failed_video_send_does_not_report_successful_fetch() {
        let ready = Rc::new(Cell::new(true));
        let notified = Rc::new(Cell::new(false));
        let mut result = Box::pin(complete_video_send(
            GatedSend {
                ready: ready.clone(),
                fail: true,
            },
            {
                let notified = notified.clone();
                move || notified.set(true)
            },
        ));
        let waker = noop_waker();
        let mut context = Context::from_waker(&waker);

        assert!(matches!(
            result.as_mut().poll(&mut context),
            Poll::Ready(Err("send failed"))
        ));
        assert!(!notified.get());
    }
}
