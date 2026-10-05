use serde_json::{Map, Value};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
struct Trace;
static ACTIVE: AtomicBool = AtomicBool::new(false);
static MAX: AtomicUsize = AtomicUsize::new(0);
static REQUESTED: AtomicUsize = AtomicUsize::new(0);
static COUNT: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Trace {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if ACTIVE.load(Relaxed) {
            MAX.fetch_max(l.size(), Relaxed);
            REQUESTED.fetch_add(l.size(), Relaxed);
            COUNT.fetch_add(1, Relaxed);
        }
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static ALLOC: Trace = Trace;
fn reset() {
    MAX.store(0, Relaxed);
    REQUESTED.store(0, Relaxed);
    COUNT.store(0, Relaxed);
}
fn main() {
    println!(
        "Value={} String={} usize={}",
        std::mem::size_of::<Value>(),
        std::mem::size_of::<String>(),
        std::mem::size_of::<usize>()
    );
    assert!(std::mem::size_of::<Value>() <= 80);
    for n in 0..=448 {
        let mut array = Vec::new();
        for _ in 0..n {
            array.push(Value::Null);
        }
        assert!(array.capacity() <= 2 * n + 4);
    }
    reset();
    let mut object = Map::new();
    for n in 0..448 {
        let key = n.to_string();
        ACTIVE.store(true, Relaxed);
        object.insert(key, Value::Null);
        ACTIVE.store(false, Relaxed);
    }
    println!(
        "map448 maximum_layout={} cumulative_requested={} allocations={}",
        MAX.load(Relaxed),
        REQUESTED.load(Relaxed),
        COUNT.load(Relaxed)
    );
    assert!(REQUESTED.load(Relaxed) <= 448 * 1536);
    reset();
    ACTIVE.store(true, Relaxed);
    let mut nested = Value::Null;
    for _ in 0..32 {
        let mut map = Map::new();
        map.insert(String::new(), nested);
        nested = Value::Object(map);
    }
    let cloned = nested.clone();
    ACTIVE.store(false, Relaxed);
    println!(
        "nested32_plus_clone cumulative_requested={} maximum_layout={}",
        REQUESTED.load(Relaxed),
        MAX.load(Relaxed)
    );
    assert!(REQUESTED.load(Relaxed) <= 2 * 32 * 1536);
    drop(cloned);
    println!(
        "arrays0..448 singleton_min_capacity4 geometric_envelope=PASS; both_tree_lifetimes=PASS"
    );
}
