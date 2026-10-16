// CO-RE test: field offsets come from the running kernel's BTF.
#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include <bpf/bpf_core_read.h>

char LICENSE[] SEC("license") = "GPL";

int target_tgid = 0;
__u64 kprobe_hits = 0, fentry_hits = 0, fexit_hits = 0, tp_hits = 0;
int ppid = 0, has_pid = 0, has_bogus = 0;
long fexit_ret = 0;
char comm[16] = {};

/* A field that no kernel has: bpf_core_field_exists must say 0. */
struct task_struct___bogus {
	int no_such_field;
};

SEC("kprobe/do_sys_openat2")
int BPF_KPROBE(kp_openat2)
{
	struct task_struct *t = (void *)bpf_get_current_task();

	if (BPF_CORE_READ(t, tgid) != target_tgid)
		return 0;
	__sync_fetch_and_add(&kprobe_hits, 1);
	ppid = BPF_CORE_READ(t, real_parent, tgid);
	BPF_CORE_READ_STR_INTO(&comm, t, comm);
	has_pid = bpf_core_field_exists(t->pid);
	has_bogus = bpf_core_field_exists(((struct task_struct___bogus *)t)->no_such_field);
	return 0;
}

SEC("fentry/do_sys_openat2")
int BPF_PROG(fe_openat2)
{
	struct task_struct *t = bpf_get_current_task_btf();

	if (t->tgid == target_tgid)
		__sync_fetch_and_add(&fentry_hits, 1);
	return 0;
}

SEC("fexit/do_sys_openat2")
int BPF_PROG(fx_openat2, int dfd, const char *filename, struct open_how *how, long ret)
{
	struct task_struct *t = bpf_get_current_task_btf();

	if (t->tgid == target_tgid) {
		__sync_fetch_and_add(&fexit_hits, 1);
		fexit_ret = ret;
	}
	return 0;
}

SEC("tp_btf/sys_enter")
int BPF_PROG(tp_sys_enter, struct pt_regs *regs, long id)
{
	struct task_struct *t = bpf_get_current_task_btf();

	if (t->tgid == target_tgid && id == 56 /* openat */)
		__sync_fetch_and_add(&tp_hits, 1);
	return 0;
}
