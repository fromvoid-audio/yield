//! Isolated allocator proof against the actual private publication custody wrapper.
#[path = "../crates/yield-library/src/operation_journal/owner/publication_fence/custody.rs"]
mod custody;
use custody::Custody;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Barrier};

static ARMED: AtomicIsize = AtomicIsize::new(-1);
static POINTERS: [AtomicUsize; 3] = [const { AtomicUsize::new(0) }; 3];
static FREED: [AtomicBool; 3] = [const { AtomicBool::new(false) }; 3];
static REFUNDS: AtomicUsize = AtomicUsize::new(0);
struct TrackedAllocator;
unsafe impl GlobalAlloc for TrackedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        let index = ARMED.swap(-1, SeqCst);
        if index >= 0 {
            POINTERS[index as usize].store(pointer as usize, SeqCst);
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        for (index, tracked) in POINTERS.iter().enumerate() {
            if tracked.load(SeqCst) == pointer as usize {
                FREED[index].store(true, SeqCst);
            }
        }
    }
}
#[global_allocator]
static ALLOCATOR: TrackedAllocator = TrackedAllocator;
struct Credit;
impl Drop for Credit {
    fn drop(&mut self) {
        for (index, pointer) in POINTERS.iter().enumerate() {
            if pointer.load(SeqCst) != 0 {
                assert!(
                    FREED[index].load(SeqCst),
                    "credit refunded before allocation {index} freed"
                );
            }
        }
        REFUNDS.fetch_add(1, SeqCst);
    }
}
struct Graph {
    _payload: Vec<u8>,
    _credit: Custody<Credit>,
}
fn reset() {
    for pointer in &POINTERS {
        pointer.store(0, SeqCst);
    }
    for freed in &FREED {
        freed.store(false, SeqCst);
    }
    REFUNDS.store(0, SeqCst);
}
fn main() {
    // Negative control: ordinary Arc drops its value before freeing its header.
    ARMED.store(0, SeqCst);
    let raw = Arc::new(Credit);
    assert!(std::panic::catch_unwind(|| drop(raw)).is_err());
    assert_eq!(REFUNDS.load(SeqCst), 0);
    assert!(FREED[0].load(SeqCst));
    for _ in 0..64 {
        reset();
        ARMED.store(0, SeqCst);
        let credit = Custody::new(Credit);
        ARMED.store(1, SeqCst);
        let payload = vec![0u8; 1024];
        ARMED.store(2, SeqCst);
        let original = Custody::new(Graph {
            _payload: payload,
            _credit: credit,
        });
        let barrier = Arc::new(Barrier::new(33));
        let aliases: Vec<_> = (0..32).map(|_| original.clone()).collect();
        assert_eq!(original.strong_count(), 33);
        assert!(original.ptr_eq(&aliases[0]));
        std::thread::scope(|scope| {
            for alias in aliases {
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    drop(alias);
                });
            }
            drop(original);
            assert_eq!(REFUNDS.load(SeqCst), 0);
            assert!(FREED.iter().all(|freed| !freed.load(SeqCst)));
            barrier.wait();
        });
        assert_eq!(REFUNDS.load(SeqCst), 1);
        assert!(FREED.iter().all(|freed| freed.load(SeqCst)));
    }
    println!(
        "publication custody allocation probe: negative control detected; 64 concurrent last-drop rounds PASS"
    );
}
