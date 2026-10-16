// Loads core.bpf.o through its light skeleton: the kernel applies the
// CO-RE relocations against its own BTF. Then opens files and checks
// what the programs saw.
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <linux/perf_event.h>
#include "core.lskel.h"

static int attach_kprobe(int prog_fd, const char *func)
{
	struct perf_event_attr a = {};
	FILE *f = fopen("/sys/bus/event_source/devices/kprobe/type", "r");
	int type, pfd;

	if (!f || fscanf(f, "%d", &type) != 1)
		return -1;
	fclose(f);
	a.size = sizeof(a);
	a.type = type;
	a.config1 = (unsigned long)func;
	pfd = syscall(__NR_perf_event_open, &a, -1, 0, -1, PERF_FLAG_FD_CLOEXEC);
	if (pfd < 0 || ioctl(pfd, PERF_EVENT_IOC_SET_BPF, prog_fd) ||
	    ioctl(pfd, PERF_EVENT_IOC_ENABLE, 0))
		return -1;
	return pfd;
}

int main(void)
{
	struct core *skel = core__open();
	int i, fd, ok = 1;

	if (!skel)
		return printf("open failed\n"), 1;
	skel->bss->target_tgid = getpid();
	if (core__load(skel))
		return printf("load failed: %s\n", strerror(errno)), 1;
	if (core__attach(skel) < 0)
		return printf("attach failed: %s\n", strerror(errno)), 1;
	if (attach_kprobe(skel->progs.kp_openat2.prog_fd, "do_sys_openat2") < 0)
		return printf("kprobe attach failed: %s\n", strerror(errno)), 1;

	/* Attaching opened files too; count only the opens below. */
	skel->bss->kprobe_hits = skel->bss->fentry_hits = 0;
	skel->bss->fexit_hits = skel->bss->tp_hits = 0;
	for (i = 0; i < 5; i++) {
		fd = open("/etc/hostname", O_RDONLY);
		close(fd);
	}
	open("/no/such/file", O_RDONLY);

	struct core__bss *b = skel->bss;
#define CHECK(c) do { int r = (c); ok &= r; printf("%-4s %s\n", r ? "ok" : "FAIL", #c); } while (0)
	printf("kprobe=%llu fentry=%llu fexit=%llu tp=%llu ppid=%d comm=%s has_pid=%d has_bogus=%d fexit_ret=%ld\n",
	       b->kprobe_hits, b->fentry_hits, b->fexit_hits, b->tp_hits, b->ppid, b->comm,
	       b->has_pid, b->has_bogus, b->fexit_ret);
	CHECK(b->kprobe_hits == 6);
	CHECK(b->fentry_hits == 6);
	CHECK(b->fexit_hits == 6);
	CHECK(b->tp_hits == 6);
	CHECK(b->ppid == getppid());
	CHECK(strcmp(b->comm, "bpfcore") == 0);
	CHECK(b->has_pid == 1 && b->has_bogus == 0);
	CHECK(b->fexit_ret == -ENOENT);
	printf("%s\n", ok ? "CO-RE: PASS" : "CO-RE: FAIL");
	return !ok;
}
