/*
 * vmagent: runs shell commands for the host over vsock.
 *
 * The agent keeps one shell running and runs each command the host
 * sends in it, so the working directory, variables, functions and
 * background jobs of one command are there for the next. The host
 * connects to vsock port 1024 (VMAGENT_PORT) and exchanges frames of
 * a type byte, a little-endian u32 payload length and the payload:
 *
 *   guest -> host  'H' hello, payload "vmagent 1"
 *   host -> guest  'X' run the payload as a shell script
 *   guest -> host  'O' stdout data, 'E' stderr data
 *   guest -> host  'R' exit status of the script, i32
 *   guest -> host  'F' error message; the script did not run
 *
 * One connection is served at a time; a command keeps running if the
 * connection closes, and its output is dropped.
 *
 * Run as /init of an initramfs (as PID 1), vmagent sets up the
 * system: the root is an overlay of a tmpfs on the read-only disk
 * /dev/vda, so that all file changes are in RAM and part of a VM
 * snapshot. It installs itself as /sbin/vmagent, adds itself to
 * /etc/inittab and starts the disk's /sbin/init, a busybox init.
 *
 * With vmagent.init=none on the kernel command line, the disk's init
 * does not run: vmagent mounts the API filesystems, brings up the
 * loopback interface, and stays PID 1, starting the agent and reaping
 * orphaned processes. This suits disks made from container images,
 * which have no init.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>
#include <net/if.h>
#include <sys/ioctl.h>
#include <linux/vm_sockets.h>

#define VMAGENT_PORT 1024
#define CMD_DIR "/run/vmagent"
#define CMD_FILE CMD_DIR "/cmd"
#define MAX_SCRIPT (16 << 20)

static void logmsg(const char *fmt, ...)
{
	va_list ap;
	va_start(ap, fmt);
	fprintf(stderr, "vmagent: ");
	vfprintf(stderr, fmt, ap);
	fprintf(stderr, "\n");
	va_end(ap);
}

/* ---- init ---- */

static void die(const char *what)
{
	logmsg("%s: %s", what, strerror(errno));
	/* PID 1 must not exit; leave the system for the console. */
	for (;;)
		pause();
}

static int copy_file(const char *src, const char *dst, mode_t mode)
{
	char buf[65536];
	int in = open(src, O_RDONLY | O_CLOEXEC);
	if (in < 0)
		return -1;
	int out = open(dst, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, mode);
	if (out < 0) {
		close(in);
		return -1;
	}
	ssize_t n;
	while ((n = read(in, buf, sizeof(buf))) > 0) {
		if (write(out, buf, n) != n) {
			n = -1;
			break;
		}
	}
	close(in);
	close(out);
	return n < 0 ? -1 : 0;
}

/* Returns the value of vmagent.init= on the kernel command line, or
 * NULL. */
static char *init_param(void)
{
	static char buf[4096];
	int fd = open("/proc/cmdline", O_RDONLY);
	if (fd < 0)
		return NULL;
	ssize_t n = read(fd, buf, sizeof(buf) - 1);
	close(fd);
	if (n <= 0)
		return NULL;
	buf[n] = 0;
	for (char *tok = strtok(buf, " \n"); tok; tok = strtok(NULL, " \n")) {
		if (strncmp(tok, "vmagent.init=", 13) == 0)
			return tok + 13;
	}
	return NULL;
}

static void mount_fs(const char *src, const char *dir, const char *type,
		     unsigned long flags, const char *data)
{
	mkdir(dir, 0755);
	if (mount(src, dir, type, flags, data) < 0)
		logmsg("mount %s: %s", dir, strerror(errno));
}

static void loopback_up(void)
{
	int fd = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
	struct ifreq ifr = { 0 };
	strcpy(ifr.ifr_name, "lo");
	if (fd < 0 || ioctl(fd, SIOCGIFFLAGS, &ifr) < 0)
		goto out;
	ifr.ifr_flags |= IFF_UP | IFF_RUNNING;
	if (ioctl(fd, SIOCSIFFLAGS, &ifr) < 0)
		logmsg("lo: %s", strerror(errno));
out:
	if (fd >= 0)
		close(fd);
}

/* PID 1 without a system init: starts the agent, restarts it if it
 * exits, and reaps all other processes. */
static void run_init(void)
{
	mount_fs("proc", "/proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL);
	mount_fs("sysfs", "/sys", "sysfs", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL);
	mount_fs("devpts", "/dev/pts", "devpts", MS_NOSUID | MS_NOEXEC, "gid=5,mode=620,ptmxmode=666");
	mount_fs("tmpfs", "/dev/shm", "tmpfs", MS_NOSUID | MS_NODEV, "mode=1777");
	mount_fs("tmpfs", "/run", "tmpfs", MS_NOSUID | MS_NODEV, "mode=755");
	symlink("/proc/self/fd", "/dev/fd");
	symlink("/proc/self/fd/0", "/dev/stdin");
	symlink("/proc/self/fd/1", "/dev/stdout");
	symlink("/proc/self/fd/2", "/dev/stderr");
	sethostname("vm", 2);
	loopback_up();
	for (;;) {
		pid_t agent = fork();
		if (agent == 0) {
			execl("/sbin/vmagent", "vmagent", (char *)NULL);
			_exit(127);
		}
		int wstatus;
		pid_t pid;
		while ((pid = wait(&wstatus)) != agent) {
			if (pid < 0 && errno == ECHILD)
				break;
		}
		logmsg("agent exited with status %d; restarting", wstatus);
		sleep(1);
	}
}

static void init(void)
{
	mkdir("/dev", 0755);
	if (mount("devtmpfs", "/dev", "devtmpfs", 0, NULL) < 0)
		die("mount /dev");
	int fd = open("/dev/console", O_RDWR);
	if (fd >= 0) {
		dup2(fd, 0);
		dup2(fd, 1);
		dup2(fd, 2);
		if (fd > 2)
			close(fd);
	}

	/* The disk may still be probed. */
	struct stat st;
	for (int i = 0; i < 1000 && stat("/dev/vda", &st) < 0; i++)
		usleep(10000);

	mkdir("/lower", 0755);
	if (mount("/dev/vda", "/lower", "ext4", MS_RDONLY, NULL) < 0)
		die("mount /dev/vda");
	mkdir("/rw", 0755);
	if (mount("tmpfs", "/rw", "tmpfs", 0, "mode=0755") < 0)
		die("mount tmpfs");
	mkdir("/rw/upper", 0755);
	mkdir("/rw/work", 0755);
	mkdir("/newroot", 0755);
	if (mount("overlay", "/newroot", "overlay", 0,
		  "lowerdir=/lower,upperdir=/rw/upper,workdir=/rw/work") < 0)
		die("mount overlay");

	if (copy_file("/init", "/newroot/sbin/vmagent", 0755) < 0)
		die("install /sbin/vmagent");
	mkdir("/proc", 0755);
	mount("proc", "/proc", "proc", 0, NULL);
	char *init_path = init_param();
	if (!init_path)
		init_path = "/sbin/init";
	int no_init = strcmp(init_path, "none") == 0;
	if (!no_init) {
		fd = open("/newroot/etc/inittab", O_WRONLY | O_APPEND);
		if (fd < 0)
			die("/etc/inittab");
		const char *entry = "\n# Host command agent\n::respawn:/sbin/vmagent\n";
		if (write(fd, entry, strlen(entry)) < 0)
			die("/etc/inittab");
		close(fd);
	}
	umount("/proc");

	if (mount("/dev", "/newroot/dev", NULL, MS_MOVE, NULL) < 0)
		die("move /dev");
	if (chdir("/newroot") < 0 || mount(".", "/", NULL, MS_MOVE, NULL) < 0 ||
	    chroot(".") < 0 || chdir("/") < 0)
		die("switch root");
	if (no_init)
		run_init();
	execl(init_path, init_path, (char *)NULL);
	die("exec init");
}

/* ---- agent ---- */

struct shell {
	pid_t pid;
	int in, out, err, status;
	char status_buf[32];
	size_t status_len;
};

static struct shell sh = { .pid = -1, .in = -1, .out = -1, .err = -1, .status = -1 };
static int conn = -1;
static int running;

static void close_fd(int *fd)
{
	if (*fd >= 0)
		close(*fd);
	*fd = -1;
}

static int write_all(int fd, const void *buf, size_t len)
{
	const char *p = buf;
	while (len > 0) {
		ssize_t n = write(fd, p, len);
		if (n < 0) {
			if (errno == EINTR)
				continue;
			return -1;
		}
		p += n;
		len -= n;
	}
	return 0;
}

static void send_frame(char type, const void *data, uint32_t len)
{
	if (conn < 0)
		return;
	unsigned char hdr[5] = { type, len, len >> 8, len >> 16, len >> 24 };
	if (write_all(conn, hdr, 5) < 0 || write_all(conn, data, len) < 0) {
		logmsg("host connection: %s", strerror(errno));
		close_fd(&conn);
	}
}

static void send_status(int status)
{
	unsigned char b[4] = { status, status >> 8, status >> 16, status >> 24 };
	send_frame('R', b, 4);
}

static void send_error(const char *fmt, ...)
{
	char msg[512];
	va_list ap;
	va_start(ap, fmt);
	vsnprintf(msg, sizeof(msg), fmt, ap);
	va_end(ap);
	send_frame('F', msg, strlen(msg));
}

/* Returns the shell exit status of a wait status. */
static int exit_code(int wstatus)
{
	if (WIFEXITED(wstatus))
		return WEXITSTATUS(wstatus);
	if (WIFSIGNALED(wstatus))
		return 128 + WTERMSIG(wstatus);
	return 255;
}

static void child_env(void)
{
	signal(SIGPIPE, SIG_DFL);
	setenv("HOME", "/root", 1);
	setenv("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", 1);
	setenv("SHELL", "/bin/sh", 1);
	setenv("USER", "root", 1);
	setenv("LOGNAME", "root", 1);
	setenv("TERM", "dumb", 1);
}

static int start_shell(void)
{
	int in[2], out[2], err[2], st[2];
	if (pipe2(in, O_CLOEXEC) < 0 || pipe2(out, O_CLOEXEC) < 0 ||
	    pipe2(err, O_CLOEXEC) < 0 || pipe2(st, O_CLOEXEC) < 0) {
		logmsg("pipe: %s", strerror(errno));
		return -1;
	}
	pid_t pid = fork();
	if (pid < 0) {
		logmsg("fork: %s", strerror(errno));
		return -1;
	}
	if (pid == 0) {
		/* dup2 clears close-on-exec. */
		dup2(in[0], 0);
		dup2(out[1], 1);
		dup2(err[1], 2);
		dup2(st[1], 3);
		setsid();
		child_env();
		if (chdir("/root") < 0)
			chdir("/");
		execl("/bin/sh", "sh", (char *)NULL);
		_exit(127);
	}
	close(in[0]);
	close(out[1]);
	close(err[1]);
	close(st[1]);
	sh.pid = pid;
	sh.in = in[1];
	sh.out = out[0];
	sh.err = err[0];
	sh.status = st[0];
	sh.status_len = 0;
	fcntl(sh.out, F_SETFL, O_NONBLOCK);
	fcntl(sh.err, F_SETFL, O_NONBLOCK);
	return 0;
}

/* Forwards the available output of the shell. Returns 0 at EOF. */
static int forward(int fd, char type)
{
	char buf[65536];
	ssize_t n = read(fd, buf, sizeof(buf));
	if (n > 0 && running)
		send_frame(type, buf, n);
	if (n == 0)
		return 0;
	return 1;
}

static void drain(void)
{
	char buf[65536];
	for (int i = 0; i < 2; i++) {
		int fd = i == 0 ? sh.out : sh.err;
		ssize_t n;
		while (fd >= 0 && (n = read(fd, buf, sizeof(buf))) > 0) {
			if (running)
				send_frame(i == 0 ? 'O' : 'E', buf, n);
		}
	}
}

/* The shell exited: reports the running command and starts a new
 * shell. */
static void shell_died(void)
{
	int wstatus = 0;
	drain();
	close_fd(&sh.in);
	close_fd(&sh.out);
	close_fd(&sh.err);
	close_fd(&sh.status);
	if (sh.pid > 0)
		waitpid(sh.pid, &wstatus, 0);
	sh.pid = -1;
	if (running) {
		running = 0;
		send_status(exit_code(wstatus));
	}
	start_shell();
}

/* Checks the syntax of the script, so that a syntax error does not
 * end the shell. Returns 0 if the script parses. */
static int check_syntax(void)
{
	int err[2];
	if (pipe2(err, O_CLOEXEC) < 0)
		return 0;
	pid_t pid = fork();
	if (pid < 0) {
		close(err[0]);
		close(err[1]);
		return 0;
	}
	if (pid == 0) {
		int null = open("/dev/null", O_RDWR);
		dup2(null, 0);
		dup2(null, 1);
		dup2(err[1], 2);
		child_env();
		execl("/bin/sh", "sh", "-n", CMD_FILE, (char *)NULL);
		_exit(0);
	}
	close(err[1]);
	char msg[4096];
	size_t len = 0;
	ssize_t n;
	while ((n = read(err[0], msg + len, sizeof(msg) - len)) > 0 && len < sizeof(msg))
		len += n;
	close(err[0]);
	int wstatus;
	waitpid(pid, &wstatus, 0);
	if (exit_code(wstatus) == 0)
		return 0;
	running = 1;
	if (len > 0)
		send_frame('E', msg, len);
	running = 0;
	send_status(exit_code(wstatus));
	return -1;
}

static void run_script(const char *script, uint32_t len)
{
	if (running) {
		send_error("a command is running");
		return;
	}
	if (sh.pid < 0 && start_shell() < 0) {
		send_error("cannot start the shell");
		return;
	}
	mkdir(CMD_DIR, 0700);
	int fd = open(CMD_FILE, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
	if (fd < 0 || write_all(fd, script, len) < 0 || write_all(fd, "\n", 1) < 0) {
		send_error("%s: %s", CMD_FILE, strerror(errno));
		if (fd >= 0)
			close(fd);
		return;
	}
	close(fd);
	if (check_syntax() < 0)
		return;
	/* The script runs in the shell itself; it gets no stdin and not
	 * the status descriptor. */
	static const char line[] = "{ . " CMD_FILE "\n} </dev/null 3>&-; echo $? >&3\n";
	running = 1;
	if (write_all(sh.in, line, sizeof(line) - 1) < 0)
		shell_died();
}

/* Reads the exit status line of the command from the shell. */
static void read_status(void)
{
	ssize_t n = read(sh.status, sh.status_buf + sh.status_len,
			 sizeof(sh.status_buf) - 1 - sh.status_len);
	if (n <= 0) {
		shell_died();
		return;
	}
	sh.status_len += n;
	char *nl;
	while ((nl = memchr(sh.status_buf, '\n', sh.status_len)) != NULL) {
		*nl = 0;
		int status = atoi(sh.status_buf);
		size_t used = nl + 1 - sh.status_buf;
		memmove(sh.status_buf, nl + 1, sh.status_len - used);
		sh.status_len -= used;
		if (running) {
			drain();
			running = 0;
			send_status(status);
		}
	}
	if (sh.status_len == sizeof(sh.status_buf) - 1)
		sh.status_len = 0;
}

/* Request frames from the host. */
static unsigned char *req;
static size_t req_len, req_cap;

static void conn_input(void)
{
	if (req_cap - req_len < 65536) {
		req_cap = req_cap * 2 + 65536;
		req = realloc(req, req_cap);
		if (!req) {
			logmsg("out of memory");
			exit(1);
		}
	}
	ssize_t n = read(conn, req + req_len, req_cap - req_len);
	if (n <= 0) {
		if (n < 0 && errno == EINTR)
			return;
		close_fd(&conn);
		req_len = 0;
		return;
	}
	req_len += n;
	while (req_len >= 5) {
		uint32_t len = req[1] | req[2] << 8 | req[3] << 16 | (uint32_t)req[4] << 24;
		if (len > MAX_SCRIPT) {
			send_error("script of %u bytes is too large", len);
			close_fd(&conn);
			req_len = 0;
			return;
		}
		if (req_len < 5 + (size_t)len)
			return;
		if (req[0] == 'X')
			run_script((char *)req + 5, len);
		else
			send_error("unknown request '%c'", req[0]);
		memmove(req, req + 5 + len, req_len - 5 - len);
		req_len -= 5 + len;
		if (conn < 0)
			return;
	}
}

static int agent(int port)
{
	signal(SIGPIPE, SIG_IGN);
	int lfd = socket(AF_VSOCK, SOCK_STREAM | SOCK_CLOEXEC, 0);
	if (lfd < 0) {
		logmsg("vsock socket: %s", strerror(errno));
		return 1;
	}
	struct sockaddr_vm addr = {
		.svm_family = AF_VSOCK,
		.svm_cid = VMADDR_CID_ANY,
		.svm_port = port,
	};
	if (bind(lfd, (struct sockaddr *)&addr, sizeof(addr)) < 0 || listen(lfd, 8) < 0) {
		logmsg("vsock port %d: %s", port, strerror(errno));
		return 1;
	}
	if (start_shell() < 0)
		return 1;
	logmsg("listening on vsock port %d", port);

	for (;;) {
		struct pollfd p[5] = {
			{ .fd = lfd, .events = POLLIN },
			{ .fd = conn, .events = POLLIN },
			{ .fd = sh.out, .events = POLLIN },
			{ .fd = sh.err, .events = POLLIN },
			{ .fd = sh.status, .events = POLLIN },
		};
		if (poll(p, 5, -1) < 0) {
			if (errno == EINTR)
				continue;
			logmsg("poll: %s", strerror(errno));
			return 1;
		}
		if (p[2].revents && sh.out >= 0 && !forward(sh.out, 'O'))
			close_fd(&sh.out);
		if (p[3].revents && sh.err >= 0 && !forward(sh.err, 'E'))
			close_fd(&sh.err);
		if (p[4].revents && sh.status >= 0)
			read_status();
		if (p[1].revents && conn >= 0)
			conn_input();
		if (p[0].revents) {
			int fd = accept4(lfd, NULL, NULL, SOCK_CLOEXEC);
			if (fd < 0)
				continue;
			if (conn >= 0) {
				int open_conn = conn;
				conn = fd;
				send_error("another host connection is open");
				close_fd(&conn);
				conn = open_conn;
				continue;
			}
			conn = fd;
			req_len = 0;
			send_frame('H', "vmagent 1", 9);
		}
	}
}

int main(int argc, char **argv)
{
	if (getpid() == 1) {
		init();
		return 1;
	}
	return agent(argc > 1 ? atoi(argv[1]) : VMAGENT_PORT);
}
