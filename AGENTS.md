# ClankerVM

This is the repository for ClankerVM, a CLI and a server that make it easy to deploy and run coding agents on [AWS Lambda MicroVMs](https://docs.aws.amazon.com/lambda/latest/dg/lambda-microvms-guide.html).

<!-- framer:begin rust-monorepo/conventions -->
## Coding conventions

### General

1. Prefer importing types at the top of the file, e.g. `use std::biz::baz::boo::Foo`, over writing paths such as `std::biz::baz::boo::Foo` inline.
2. Generic type parameters start with `T`, then `U`, `V`, and so on.
3. Use `thiserror` for errors, with specific error enums rather than `Box<dyn Error>`. Don't add `anyhow`.
4. Avoid locks such as `Mutex`, `Arc<Mutex<_>>` and `Semaphore`. Give each task or thread ownership of its own resources, or share them through structured concurrency or channels (the actor pattern).
5. Each crate has one way to do each thing: extend an existing API rather than adding a parallel one.
6. Do not create custom macros.
7. Comments are rare and explain why, never what. Doc comments describe public items.

### Testing

1. Use real objects where possible. When that's not possible, prefer crate-provided test utilities and in-memory fakes over mocks.
2. Use the test builder pattern described in this [post](https://jmmv.dev/2020/12/builder-pattern-for-tests.html).
3. Prefer extending an existing fake or test builder over creating a new bespoke one. Good builders and fakes are general purpose, reusable across test suites, and mimic the behavior and APIs of the "real thing".
4. Test public APIs, preferring integration tests in each crate's `tests/` directory named after the file they cover, e.g. `src/foo/bar.rs` and `tests/foo/bar_test.rs`.
5. Put helpers that only one test file uses at the bottom of that file, below the tests.
6. Return `Result` from tests and use `?` rather than `unwrap`.
7. Never put timeouts in tests. Wait on conditions, so a broken test hangs rather than flakes.
8. When fixing a bug, write a failing test first.
<!-- framer:end rust-monorepo/conventions -->
