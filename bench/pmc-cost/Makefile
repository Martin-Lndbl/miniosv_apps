# The pmc-cost benchmark as the miniOSv application.
#
#     just build apps/bench/pmc-cost -j8

app_dir := $(patsubst %/,%,$(dir $(lastword $(MAKEFILE_LIST))))

# Defaults live in src/config.hh; anything set on the make command line wins.
# PMC_COUNTERS and PMC_WORK_STEPS are gone with the region sweep.
pmc-knobs := PMC_REPS PMC_BUDGET_NS PMC_PROBE_ITERS PMC_MIN_ITERS \
             PMC_MAX_ITERS PMC_CALIBRATE_TRIES PMC_CALIBRATE_MS \
             PMC_POWEROFF
pmc-defines := $(foreach k,$(pmc-knobs),$(if $($(k)),-D$(k)=$($(k))))

# Changing a knob changes CXXFLAGS but no source file, and make compares
# timestamps only -- without this the old object survives with the old value
# baked in. Same idiom as app_mode.last in the kernel Makefile.
pmc-cost-flags := $(out)/app-objs/pmc-cost.flags
.PHONY: pmc-cost-flags-phony
$(pmc-cost-flags): pmc-cost-flags-phony
	$(call very-quiet, $(makedir))
	@if [ "$$$$(cat $@ 2>/dev/null)" != "$(pmc-defines)" ]; then \
		echo -n "$(pmc-defines)" > $@; \
	fi

$(out)/app-objs/pmc-cost.o: CXXFLAGS += -I$(app_dir)/src -I$(app_dir)/../common $(pmc-defines)

$(out)/app-objs/pmc-cost.o: $(app_dir)/src/main.cc $(app_dir)/src/config.hh $(pmc-cost-flags) \
                          $(app_dir)/../common/bench.hh
	$(makedir)
	$(call quiet, $(CXX) $(CXXFLAGS) -c -o $@ $<, CXX pmc-cost/main.cc)

app-objects = app-objs/pmc-cost.o
