//! A closed-loop gRPC load generator for grpc_bench.
//!
//! Every worker sends a call, awaits and decodes the reply, and sends the next one. Each client
//! thread runs its own single-threaded Tokio runtime with its own connections, so the client scales
//! with CPUs the way several independent client processes do. The report mimics the summary `ghz`
//! prints, so the same analysis scripts work for both clients.

use std::collections::BTreeMap;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use hello_world::greeter_client::GreeterClient;
use hello_world::HelloRequest;
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage};
use tonic::transport::Channel;

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

pub mod hello_world {
    tonic::include_proto!("helloworld");
}

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

static DESCRIPTOR_SET: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/descriptor.bin"));

struct Options {
    target: String,
    connections: usize,
    concurrency: usize,
    cpus: usize,
    duration: Duration,
    warmup: Duration,
    data_file: String,
}

#[derive(Default)]
struct Stats {
    latencies_us: Vec<u32>,
    // (status code, message) -> count
    errors: BTreeMap<(String, String), u64>,
}

impl Stats {
    fn merge(&mut self, other: Stats) {
        self.latencies_us.extend(other.latencies_us);
        for (key, count) in other.errors {
            *self.errors.entry(key).or_default() += count;
        }
    }
}

fn main() {
    let options = parse_options();
    let request = load_request(&options.data_file);

    // Connection i belongs to thread i % threads, worker w uses connection w % connections
    let thread_count = options.cpus.min(options.connections).max(1);
    let barrier = Arc::new(Barrier::new(thread_count));
    let mut threads = Vec::new();
    for thread_index in 0..thread_count {
        let connection_count = (thread_index..options.connections).step_by(thread_count).count();
        let worker_counts: Vec<usize> = (thread_index..options.connections)
            .step_by(thread_count)
            .map(|connection| (connection..options.concurrency).step_by(options.connections).count())
            .collect();
        debug_assert_eq!(worker_counts.len(), connection_count);
        let (target, request, barrier) = (options.target.clone(), request.clone(), barrier.clone());
        let (warmup, duration) = (options.warmup, options.duration);
        threads.push(std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run_thread(target, request, worker_counts, barrier, warmup, duration))
        }));
    }

    let mut stats = Stats::default();
    for thread in threads {
        stats.merge(thread.join().unwrap());
    }
    print_report(&mut stats, options.duration);
}

async fn run_thread(
    target: String,
    request: HelloRequest,
    worker_counts: Vec<usize>,
    barrier: Arc<Barrier>,
    warmup: Duration,
    duration: Duration,
) -> Stats {
    let mut channels = Vec::new();
    for _ in 0..worker_counts.len() {
        let channel = Channel::from_shared(target.clone())
            .unwrap()
            .tcp_nodelay(true)
            .connect()
            .await
            .unwrap_or_else(|e| panic!("can't connect to {target}: {e}"));
        channels.push(channel);
    }

    // All threads start the run together, once every connection is established
    barrier.wait();
    let measure_at = Instant::now() + warmup;
    let end_at = measure_at + duration;

    let mut tasks = Vec::new();
    for (channel, worker_count) in channels.into_iter().zip(worker_counts) {
        for _ in 0..worker_count {
            let mut client = GreeterClient::new(channel.clone());
            let request = request.clone();
            tasks.push(tokio::spawn(async move {
                let mut stats = Stats::default();
                loop {
                    let started_at = Instant::now();
                    if started_at >= end_at {
                        break;
                    }
                    // A server that stops answering must not hang the client forever
                    let call = client.say_hello(request.clone());
                    let Ok(reply) = tokio::time::timeout_at((end_at + SHUTDOWN_GRACE).into(), call).await else {
                        let key = ("DeadlineExceeded".to_string(), "no reply by the end of the run".to_string());
                        *stats.errors.entry(key).or_default() += 1;
                        break;
                    };
                    if started_at < measure_at {
                        continue;
                    }
                    match reply {
                        Ok(_) => stats.latencies_us.push(started_at.elapsed().as_micros() as u32),
                        Err(status) => {
                            let key = (format!("{:?}", status.code()), status.message().to_string());
                            *stats.errors.entry(key).or_default() += 1;
                        }
                    }
                }
                stats
            }));
        }
    }

    let mut stats = Stats::default();
    for task in tasks {
        stats.merge(task.await.unwrap());
    }
    stats
}

fn load_request(data_file: &str) -> HelloRequest {
    // The payload is the JSON form of HelloRequest, the same file `ghz --data-file` takes
    let json = std::fs::read_to_string(data_file).unwrap_or_else(|e| panic!("can't read {data_file}: {e}"));
    let pool = DescriptorPool::decode(DESCRIPTOR_SET).unwrap();
    let descriptor = pool.get_message_by_name("helloworld.HelloRequest").unwrap();
    let mut deserializer = serde_json::Deserializer::from_str(&json);
    let dynamic = DynamicMessage::deserialize(descriptor, &mut deserializer)
        .unwrap_or_else(|e| panic!("{data_file} isn't a valid HelloRequest: {e}"));
    HelloRequest::decode(dynamic.encode_to_vec().as_slice()).unwrap()
}

fn print_report(stats: &mut Stats, duration: Duration) {
    let latencies = &mut stats.latencies_us;
    latencies.sort_unstable();
    let count = latencies.len();
    let ms = |us: f64| format!("{:.2} ms", us / 1000.0);
    let percentile = |q: f64| if count == 0 { 0.0 } else { latencies[((count - 1) as f64 * q) as usize] as f64 };
    let average = if count == 0 { 0.0 } else { latencies.iter().map(|&x| x as f64).sum::<f64>() / count as f64 };

    println!();
    println!("Summary:");
    let error_count: u64 = stats.errors.values().sum();
    println!("  Count:\t{}", count as u64 + error_count);
    println!("  Total:\t{:.2} s", duration.as_secs_f64());
    println!("  Slowest:\t{}", ms(percentile(1.0)));
    println!("  Fastest:\t{}", ms(percentile(0.0)));
    println!("  Average:\t{}", ms(average));
    println!("  Requests/sec:\t{:.2}", count as f64 / duration.as_secs_f64());
    println!();
    println!("Latency distribution:");
    for q in [10, 25, 50, 75, 90, 95, 99] {
        println!("  {q} % in {}", ms(percentile(q as f64 / 100.0)));
    }
    println!();
    println!("Status code distribution:");
    println!("  [OK]            {count} responses");
    let mut codes: BTreeMap<&str, u64> = BTreeMap::new();
    for ((code, _), n) in &stats.errors {
        *codes.entry(code).or_default() += n;
    }
    for (code, n) in codes {
        println!("  [{code}]   {n} responses");
    }
    if !stats.errors.is_empty() {
        println!();
        println!("Error distribution:");
        for ((code, message), n) in &stats.errors {
            println!("  [{n}]   rpc error: code = {code} desc = {message}");
        }
    }
}

fn parse_options() -> Options {
    let mut options = Options {
        target: String::new(),
        connections: 1,
        concurrency: 50,
        cpus: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        duration: Duration::from_secs(20),
        warmup: Duration::ZERO,
        data_file: "/payload/payload".to_string(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let (name, inline_value) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name.to_string(), Some(value.to_string())),
            _ => (arg.clone(), None),
        };
        if !name.starts_with("--") {
            options.target = if name.contains("://") { name } else { format!("http://{name}") };
            continue;
        }
        let value = inline_value
            .or_else(|| args.next())
            .unwrap_or_else(|| usage(&format!("{name} needs a value")));
        match name.as_str() {
            "--connections" => options.connections = parse_number(&name, &value),
            "--concurrency" => options.concurrency = parse_number(&name, &value),
            "--cpus" => options.cpus = parse_number(&name, &value),
            "--duration" => options.duration = parse_duration(&name, &value),
            "--warmup" => options.warmup = parse_duration(&name, &value),
            "--data-file" => options.data_file = value,
            _ => usage(&format!("unknown option {name}")),
        }
    }
    if options.target.is_empty() {
        usage("the target (host:port) is missing");
    }
    if options.concurrency < options.connections {
        usage("--concurrency must not be lower than --connections");
    }
    options
}

fn parse_number(name: &str, value: &str) -> usize {
    match value.parse() {
        Ok(number) if number > 0 => number,
        _ => usage(&format!("{name} needs a positive number, got {value}")),
    }
}

fn parse_duration(name: &str, value: &str) -> Duration {
    let (number, unit) = match value {
        v if v.ends_with("ms") => (&v[..v.len() - 2], 0.001),
        v if v.ends_with('s') => (&v[..v.len() - 1], 1.0),
        v if v.ends_with('m') => (&v[..v.len() - 1], 60.0),
        v => (v, 1.0),
    };
    match number.parse::<f64>() {
        Ok(number) if number >= 0.0 => Duration::from_secs_f64(number * unit),
        _ => usage(&format!("{name} needs a duration like 20s, got {value}")),
    }
}

fn usage(error: &str) -> ! {
    eprintln!("error: {error}");
    eprintln!(
        "usage: tonic_client [--connections N] [--concurrency N] [--cpus N] [--duration 20s] \
         [--warmup 0s] [--data-file /payload/payload] host:port"
    );
    std::process::exit(2)
}
