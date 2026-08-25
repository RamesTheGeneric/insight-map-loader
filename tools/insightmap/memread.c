// memread -- read a region of another process's memory on the Quest 1.
//
// Why this exists: /proc/PID/mem and dd EIO on the kernel's ptrace_may_access
// re-check for a running process; process_vm_readv passes it (root +
// CAP_SYS_PTRACE + SELinux permissive) WITHOUT stopping the target, so Insight
// keeps tracking while we read its heap. Falls back to PTRACE_ATTACH +
// /proc/PID/mem if process_vm_readv is refused.
//
//   memread <pid> <hexaddr> <size> <outfile>
//
// Requires: `adb root`, and SELinux permissive (`setenforce 0`) -- reading a
// system service's memory is otherwise denied even for root. See README.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <sys/uio.h>
#include <sys/ptrace.h>
#include <sys/wait.h>

int main(int argc, char** argv) {
  if (argc < 5) { fprintf(stderr, "usage: memread pid hexaddr size out\n"); return 2; }
  pid_t pid = atoi(argv[1]);
  unsigned long addr = strtoul(argv[2], 0, 16);
  size_t sz = strtoul(argv[3], 0, 10);
  char* buf = malloc(sz);
  if (!buf) { fprintf(stderr, "malloc %zu failed\n", sz); return 1; }
  struct iovec loc = {buf, sz}, rem = {(void*)addr, sz};
  ssize_t n = process_vm_readv(pid, &loc, 1, &rem, 1, 0);
  if (n < 0) {
    fprintf(stderr, "process_vm_readv: %s; trying ptrace attach\n", strerror(errno));
    if (ptrace(PTRACE_ATTACH, pid, 0, 0) < 0) { fprintf(stderr, "attach: %s\n", strerror(errno)); return 1; }
    waitpid(pid, 0, 0);
    char p[64]; snprintf(p, sizeof p, "/proc/%d/mem", pid);
    int mfd = open(p, O_RDONLY);
    n = pread(mfd, buf, sz, addr);
    if (n < 0) fprintf(stderr, "pread: %s\n", strerror(errno));
    close(mfd);
    ptrace(PTRACE_DETACH, pid, 0, 0);
    if (n < 0) return 1;
  }
  int out = open(argv[4], O_WRONLY | O_CREAT | O_TRUNC, 0666);
  if (write(out, buf, n) != n) { fprintf(stderr, "write failed\n"); return 1; }
  close(out);
  fprintf(stderr, "read %zd bytes -> %s\n", n, argv[4]);
  return 0;
}
