# miniosv_apps
Collection of apps to run on miniOSv

## Getting Started
As soon as [this PR](https://github.com/miniosv/miniosv/pull/28) is through, you may clone this repository anywhere on you machine and select a specific application to be build against your miniosv version by setting `app=/path/to/application/in/this/repo`. Relative paths work just as well.

## Submodules
Two of the apps here are whole upstream trees rather than a directory of
sources, so they are submodules of this repository:

- `miniduckdb` — DuckDB, with the miniOSv port under its own `miniosv/`
- `miniduckdb-httpfs` — the httpfs extension, upstream and unpatched

`bench/duckdb-tpch` is the app that builds them; it holds the `Makefile` that
names both checkouts and nothing else. Run `git submodule update --init` before
building it. The other apps need no submodule.
