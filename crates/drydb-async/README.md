# drydb-async

Async adapter for [`drydb`](https://github.com/hckaye/drydb-rs/tree/main/crates/drydb).

Reading a page is a blocking file read, so this crate does not pretend otherwise: a
lookup first tries the page cache on the calling thread and completes immediately if
every page it needs is already there, and otherwise hands the whole query to
`tokio::task::spawn_blocking`.

What it does not do is wrap a blocking read in an `async fn` and call it asynchronous.
