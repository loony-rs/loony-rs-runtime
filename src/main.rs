// examples/demo.rs
use loony_rs_runtime::{Runtime, spawn};

async fn hello_world() {
    println!("Hello, world!");
}

async fn compute() -> u32 {
    let mut sum = 0;
    for i in 0..1000 {
        sum += i;
        tokio::task::yield_now().await;
    }
    sum
}

#[tokio::main]
async fn main() {
    let mut rt = Runtime::new().unwrap();

    let handle = rt.spawn(async { compute().await });

    rt.block_on(async {
        hello_world().await;

        let result = handle.await.unwrap();
        println!("Computed: {}", result);
    });
}
