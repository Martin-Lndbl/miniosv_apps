# The smoltcp-s3 benchmark as the miniOSv application.
#
# The crate here is only the benchmark; the stack it drives is
# modules/mininet in the miniOSv tree, pulled in as a cargo path dependency
# and linked into the same archive. The shim that crate calls into is C++, so
# the kernel compiles it -- that is what mininet.mk contributes.

app_dir := $(patsubst %/,%,$(dir $(lastword $(MAKEFILE_LIST))))

include modules/mininet/mininet.mk

app_cargo_dir = $(out)/app-objs/cargo
app_lib = $(app_cargo_dir)/release/libosv_app.a

.PHONY: $(app_lib)
$(app_lib):
	$(call quiet, cargo build --release --manifest-path $(app_dir)/Cargo.toml \
		--target-dir $(app_cargo_dir), CARGO $(app_dir))

$(out)/app-objs/smoltcp-s3.o: $(app_lib)
	$(makedir)
	$(call quiet, cmp -s $< $@ || cp $< $@, CP libosv_app.a)

app-objects = app-objs/smoltcp-s3.o
app-objects += $(mininet-objects)
