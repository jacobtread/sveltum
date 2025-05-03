use std::{cell::RefCell, collections::VecDeque, rc::Rc, task::Waker};

/// Simple queue that invokes a waker when an item is added
/// to the queue, sharable on the same thread through [Rc]
pub struct WakerQueue<T> {
    inner: Rc<RefCell<WakerQueueInner<T>>>,
}

impl<T> Default for WakerQueue<T> {
    fn default() -> Self {
        Self {
            inner: Rc::new(RefCell::new(WakerQueueInner {
                queue: Default::default(),
                waker: None,
            })),
        }
    }
}

impl<T> Clone for WakerQueue<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone()
        }
    }
}

struct WakerQueueInner<T> {
    queue: VecDeque<T>,
    waker: Option<Waker>,
}

impl<T> WakerQueue<T> {
    /// Set the current waker
    pub fn set_waker(&self, waker: &Waker) {
        self.inner.borrow_mut().waker = Some(waker.clone());
    }

    /// Push an item to the queue and notify the current waker
    pub fn push(&self, value: T) {
        let mut inner = self.inner.borrow_mut();
        let queue = &mut inner.queue;
        queue.push_back(value);

        // Notify the waker
        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }
    }

    /// Pop the first item from the queue
    pub fn next(&self) -> Option<T> {
        let mut inner = self.inner.borrow_mut();
        let queue = &mut inner.queue;
        queue.pop_front()
    }
}
