# DuckDB as the miniOSv application.
#
#     just build apps/bench/duckdb-tpch -j8
#
# The engine is not here: it is the miniduckdb submodule at the root of this
# repository, and the httpfs extension it reads S3 through is the
# miniduckdb-httpfs one beside it. Both used to be submodules of the miniOSv
# tree; keeping them here instead leaves miniOSv's own submodules alone and
# puts every app this repository builds in one place.
#
# All this file does is say where those two checkouts are and hand off to the
# fragment miniduckdb carries for exactly this purpose. Everything else --
# the source list, the shims, the flags, the MININET_* knobs -- lives there.

app_dir := $(patsubst %/,%,$(dir $(lastword $(MAKEFILE_LIST))))
apps_dir := $(abspath $(app_dir)/../..)

duckdb-dir := $(apps_dir)/miniduckdb
httpfs-dir := $(apps_dir)/miniduckdb-httpfs

ifeq ($(wildcard $(duckdb-dir)/miniosv/miniosv.mk),)
$(error $(duckdb-dir) is empty -- run 'git submodule update --init' in $(apps_dir))
endif
ifeq ($(wildcard $(httpfs-dir)/src/httpfs.cpp),)
$(error $(httpfs-dir) is empty -- run 'git submodule update --init' in $(apps_dir))
endif

include $(duckdb-dir)/miniosv/miniosv.mk
