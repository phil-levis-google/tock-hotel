CURRENT_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))

TOCK_ARCH ?= cortex-m3
TOCK_USERLAND_BASE_DIR = $(CURRENT_DIR)../../../tock/userland
BUILDDIR ?= $(CURRENT_DIR)/build/$(TOCK_ARCH)

C_SRCS   := $(wildcard *.c)

OBJS += $(patsubst %.c,$(BUILDDIR)/%.o,$(C_SRCS))

CPPFLAGS += -DSTACK_SIZE=2048

include $(TOCK_USERLAND_BASE_DIR)/AppMakefile.mk

$(BUILDDIR)/%.o: %.c | $(BUILDDIR)
	$(CC) $(CFLAGS) $(CPPFLAGS) -c -o $@ $<

