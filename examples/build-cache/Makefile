# The generated project: one object for each src/*.c, each compiled by its own call of $(CC),
# so that sccache looks each one up in its cache. OUT is the directory of the objects.
OUT ?= out
SRCS := $(sort $(wildcard src/*.c))
OBJS := $(patsubst src/%.c,$(OUT)/%.o,$(SRCS))

all: $(OBJS)

$(OUT)/%.o: src/%.c | $(OUT)
	$(CC) -O2 -c $< -o $@

$(OUT):
	mkdir -p $@
