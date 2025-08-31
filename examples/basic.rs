use std::alloc;
use thresher::Thresher;

#[global_allocator]
static THRESHER: Thresher<alloc::System> = Thresher::new(alloc::System);

fn main() {
    THRESHER.set_threshold(100 * 1024 * 1024);
    THRESHER.set_callback(|allocation| {
        println!("Threshold reached! Allocated: {} bytes", allocation);
    });

    // This is what we're adding to our vec each loop
    let bytes = vec![0u8; 10 * 1024 * 1024];

    let mut vec = vec![0u8];

    for i in 0..10 {
        println!("Loop {i}, Vec Capacity: {}", vec.capacity());
        vec.extend(&bytes);
    }
}
