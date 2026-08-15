app_dir := $(patsubst %/,%,$(dir $(lastword $(MAKEFILE_LIST))))

app_cargo_dir = $(out)/app-objs/cargo
app_lib = $(app_cargo_dir)/release/libosv_app.a

.PHONY: $(app_lib)
$(app_lib):
	$(call quiet, cargo build --release --manifest-path $(app_dir)/Cargo.toml \
		--target-dir $(app_cargo_dir), CARGO $(app_dir))

$(out)/app-objs/smoltcp-s3.o: $(app_lib)
	$(makedir)
	$(call quiet, cmp -s $< $@ || cp $< $@, CP libosv_app.a)

$(out)/app-objs/shim/shim.o: $(app_dir)/shim/shim.cc | generated-headers $(out)/.libcxx-built
	$(makedir)
	$(call quiet, $(CXX) $(CXXFLAGS) -Iinclude/api/minidpdk \
		$(autodepend) -c -o $@ $<, CXX $(app_dir)/shim/shim.cc)

app-objects = app-objs/smoltcp-s3.o app-objs/shim/shim.o
