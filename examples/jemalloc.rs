use thresher::Thresher;
use tokio::sync::watch;

#[allow(non_upper_case_globals)]
#[unsafe(export_name = "malloc_conf")]
pub static malloc_conf: &[u8] = b"prof:true,prof_active:true,lg_prof_sample:19\0";

#[global_allocator]
static ALLOCATOR: Thresher<tikv_jemallocator::Jemalloc> =
    Thresher::new(tikv_jemallocator::Jemalloc);

#[tokio::main]
async fn main() {
    // We use this to notify the async task that the threshold has been reached
    let (tx, mut rx) = watch::channel::<()>(());

    ALLOCATOR.set_threshold(100 * 1024 * 1024);
    ALLOCATOR.set_callback(move |_| {
        tx.send(()).ok();
    });

    // This tasks waits for the threshold to be reached and
    // when it is, will dump out a heap dump to the local dir
    let watcher_task = tokio::spawn(async move {
        // don't use unwrap in actual code, this is just an example
        rx.changed().await.unwrap();
        println!("Watch notified, dumping out bytes");

        let mut prof_ctl = jemalloc_pprof::PROF_CTL.as_ref().unwrap().lock().await;
        let pprof = prof_ctl.dump_pprof().unwrap();

        // Use `pprof` from here to read this file: https://github.com/google/pprof
        tokio::fs::write("heap_profile.pb.gz", pprof).await.unwrap();
    });

    // This part is the same in the basic example
    let bytes = vec![0u8; 10 * 1024 * 1024];

    let mut vec = vec![0u8];

    for i in 0..10 {
        println!("Loop {i}, Vec Capacity: {}", vec.capacity());
        vec.extend(&bytes);
    }

    watcher_task.await.unwrap();
}
