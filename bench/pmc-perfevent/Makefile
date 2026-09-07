# The pmc-perfevent benchmark as the miniOSv application.
#
#     just build apps/bench/pmc-perfevent -j8

app_dir := $(patsubst %/,%,$(dir $(lastword $(MAKEFILE_LIST))))

# Defaults live in src/config.hh; anything set on the make command line wins.
# PMC_WORKS is absent: a braced list does not survive a shell round trip.
pmc-knobs := PMC_REPS PMC_EVENTS PMC_POWEROFF
pmc-defines := $(foreach k,$(pmc-knobs),$(if $($(k)),-D$(k)=$($(k))))

# Changing a knob changes CXXFLAGS but no source file, and make compares
# timestamps only -- without this the old object survives with the old value
# baked in. Same idiom as app_mode.last in the kernel Makefile.
pmc-perfevent-flags := $(out)/app-objs/pmc-perfevent.flags
.PHONY: pmc-perfevent-flags-phony
$(pmc-perfevent-flags): pmc-perfevent-flags-phony
	$(call very-quiet, $(makedir))
	@if [ "$$$$(cat $@ 2>/dev/null)" != "$(pmc-defines)" ]; then \
		echo -n "$(pmc-defines)" > $@; \
	fi

$(out)/app-objs/pmc-perfevent.o: CXXFLAGS += \
	-I$(app_dir)/src -I$(app_dir)/../common $(pmc-defines)

$(out)/app-objs/pmc-perfevent.o: $(app_dir)/src/main.cc $(app_dir)/src/config.hh $(pmc-perfevent-flags) \
                              $(app_dir)/../common/bench.hh \
                              $(app_dir)/../common/hashtable.hh
	$(makedir)
	$(call quiet, $(CXX) $(CXXFLAGS) -c -o $@ $<, CXX pmc-perfevent/main.cc)

app-objects = app-objs/pmc-perfevent.o
