# Axactor

**Axactor** is a **production-grade actor model library** based on **Tokio**.
It aims to provide both usability (macro-based message API generation) and operational stability (supervision/restarts, graceful shutdown, metrics, registry/reaper).

> **Core Concepts:**
> * "Actors process messages sequentially, creating the illusion of being single-threaded."
> * "The library takes responsibility for observation, termination, restarting, and cleanup required in production environments."
> 
> 

---

## Key Features

### 1) Macro-based Actor API Generation

The `#[actor]` macro automatically generates:

* `ActorNameMsg` (Message Enum)
* `ActorNameRef` (Actor Ref / Handle API)
* Implementations for `MessageKind` and `RefMetrics`

* Return type `()`: **tell** (fire-and-forget)
* Return type with value: **ask** (oneshot reply)
* `#[msg]` methods can be **sync or async**.


---

### 2) Supervision: Panic-based Restart Policy

Axactor's supervision logic relies on `std::panic::catch_unwind`.

* `RestartPolicy::None`: Terminate on panic.
* `RestartPolicy::OnPanic { max_restarts, min_backoff, max_backoff }`: Restart with backoff strategy on panic.

> **Important:** This requires `panic = "unwind"` in your `Cargo.toml`.
> If compiled with `panic = "abort"`, panics cannot be caught, making **restarts impossible**.

---

### 3) Shutdown Modes

* `ShutdownMode::Immediate`
* Upon receiving a shutdown signal, the mailbox is **closed immediately**, and the actor terminates.


* `ShutdownMode::Graceful { deadline }`
* Upon receiving a shutdown signal, the mailbox is closed (starts draining).
* The actor terminates normally once the queue is empty (`inflight == 0`).
* If the `deadline` is exceeded, it terminates forcibly.



---

### 4) Cancel-safe Send / Backpressure

`MailboxTx::send_wait` is implemented based on `mpsc::Sender::reserve()`, making it **cancel-safe**.

* `try_send`: Attempts to send immediately; returns `Full` or `Closed` error.
* `send_wait`: Sends after acquiring a permit (safe backpressure).

---

### 5) Metrics & State Tracking (MailboxMeter)

Each actor maintains internal metrics:

* `mailbox_len()`: Current queue length.
* `inflight_count()`: Number of messages currently being processed.
* `idle_for()`: Time elapsed since the last processing completion.
* `silence_for()`: Time elapsed since the last message reception.
* `incarnation()`: Restart generation number (starts at 1).
* `is_closed()`: Whether the mailbox is closed.
* `is_terminated()`: Whether the task has fully terminated.

---

### 6) Actor Lifecycle State: Closed vs Terminated

Axactor exposes two related but distinct lifecycle flags via `MailboxMeter`.

* **Closed**: The actor has closed its mailbox (no further messages will be accepted; draining may be in progress).
* **Terminated**: The actor task has fully exited and cleanup is complete.

#### State Definitions

| State | Meaning | What you can assume | Typical transitions / triggers |
| --- | --- | --- | --- |
| `is_closed() == false` | Mailbox is open | Messages may still be accepted and processed | Normal running state |
| `is_closed() == true` | Mailbox is closed | No new messages should be accepted; actor may be draining or already exiting | Graceful shutdown begins, immediate shutdown begins, `Context::stop()` requested, `rx` closed, cleanup path sets closed |
| `is_terminated() == false` | Task still alive | Actor task may still process or finish draining | Normal running / draining / shutdown in progress |
| `is_terminated() == true` | Task has exited | Actor will not run again (unless a new incarnation is spawned by supervision); registry cleanup should be done | Cleanup guard drop executes at task end |

#### Practical Interpretation

1. **Closed but not Terminated** (`closed=true`, `terminated=false`)
The actor has stopped accepting new work but the task is still alive—typically draining remaining messages or finishing shutdown hooks.
2. **Closed and Terminated** (`closed=true`, `terminated=true`)
The actor is fully gone: mailbox is closed and the underlying task has exited.
At this point, the system map entry should be removed, and registry entries should be self-evicted.

> **Note:** Do not rely on "Terminated without Closed". In Axactor’s intended semantics, termination implies the mailbox is no longer usable; the cleanup path sets `closed=true` as part of shutdown/exit.

#### Behavioral Guarantees

* **Closed** is a mailbox-level signal (“no more intake”).
* **Terminated** is a task-level signal (“the actor loop is over, cleanup completed”).
* `TellError::Closed` corresponds to mailbox refusal; it can happen as soon as **Closed** is set (or the channel is closed), even if the actor is not yet **Terminated**.

---

### 7) Registry: Keyed Actor Caching + Self-Eviction

`Registry<K, R>` provides a "Key-based Singleton Actor" mechanism.

* Calling `get_or_spawn()` with the same `key` returns the same actor reference.
* To prevent naming collisions, it generates a **hashed name** using `(base_name, shard_idx, key_hash)`.
* It attaches a guard to ensure **self-eviction** from the registry when the actor terminates.
* This handles ABA/race conditions to ensure no stale entries remain.



Additionally, `start_reaper(idle_timeout, check_interval)` allows for **automatic stopping of idle actors**.

---

## Installation

Add the following to your `Cargo.toml`:

```toml
[dependencies]
axactor = "0.1.0"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time"] }
tracing = "0.1"

```

### Axum Integration

If you enable the `axum` feature, `AskError` and `TellError` implement `IntoResponse`.

```toml
axactor = { version = "0.1.0", features = ["axum"] }

```

---

## Quick Start

### 1) Define an Actor

```rust
use axactor::{actor, Context};

pub struct Counter {
    n: i64,
}

#[actor]
impl Counter {
    pub fn new() -> Self {
        Self { n: 0 }
    }

    #[msg]
    fn inc(&mut self) {
        self.n += 1;
    }

    #[msg]
    fn add(&mut self, x: i64) {
        self.n += x;
    }

    #[msg]
    fn get(&mut self) -> i64 {
        self.n
    }

    // (Optional) Lifecycle hooks
    async fn on_start(&mut self, _ctx: &mut Context) {
        // Initialization logic
    }
}

```

> The code above automatically generates:

* `CounterMsg`
* `CounterRef` (with methods: `inc()`, `inc_wait()`, `get().await`, `get_wait().await`, etc.)

---

### 2) Spawn via System

```rust
use axactor::{System, SpawnConfig};
use std::time::Duration;

#[tokio::main]
async fn main() {
    let system = System::new();

    let cfg = SpawnConfig::new("counter", 1024)
        .restart_on_panic(10, Duration::from_millis(50), Duration::from_secs(3))
        .shutdown_graceful(Duration::from_secs(2));

    let h = system.spawn_with(|| Counter::new(), cfg).unwrap();
    let c = h.actor();

    c.inc().unwrap();
    c.add(10).unwrap();

    let v = c.get().await.unwrap();
    println!("value = {}", v);

    system.shutdown_graceful(Duration::from_secs(5)).await;
}

```

---

## Ask/Tell Error Model

### TellError

* `Full`: The mailbox is full (backpressure).
* `Closed`: The actor has closed its mailbox or has terminated.

### AskError

* `Full` / `Closed`: Same meaning as in TellError.
* `Timeout`: Occurs within the timeout wrapper.
* `Canceled`: The reply oneshot channel was dropped (e.g., actor crash or termination).

---

## Observability

Warning logs are generated if message processing takes too long.

* **> 100ms**: `warn!` (Slow message)
* **< 100ms**: `debug!`

This logging is based on `tracing`. Guard generation is determined by whether `Level::DEBUG` or `Level::WARN` is enabled.

---

## Registry Example

```rust
use axactor::{registry::Registry, SpawnConfig, System};
use std::{sync::Arc, time::Duration};

#[tokio::main]
async fn main() {
    let system = Arc::new(System::new());
    let reg: Registry<String, CounterRef> = Registry::new(system.clone());

    let cfg = SpawnConfig::new("counter", 256)
        .shutdown_graceful(Duration::from_secs(1));

    // Calling with the same key returns the same actor ref
    let a = reg.get_or_spawn("user:1".to_string(), cfg.clone(), || Counter::new()).await;
    let b = reg.get_or_spawn("user:1".to_string(), cfg.clone(), || Counter::new()).await;

    a.inc().unwrap();
    println!("{}", b.get().await.unwrap());

    // Automatically stop idle actors
    reg.start_reaper(Duration::from_secs(60), Duration::from_secs(10));
}

```

---

## Design & Policy Summary

* **Panic restarts assume `panic=unwind`.**
* **When an actor task terminates:**
* Meter: `closed=true`, `terminated=true`.
* Name removed from the system map.
* If using the Registry, the entry is cleaned up via the self-eviction guard.


* **Graceful Shutdown:**
* Closes the mailbox and attempts to drain pending messages.
* Forcibly terminates if the deadline is exceeded.



---

## License

[MIT](LICENSE)