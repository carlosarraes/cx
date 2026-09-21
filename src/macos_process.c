#include <errno.h>
#include <libproc.h>
#include <stdint.h>
#include <string.h>
#include <sys/proc.h>
#include <sys/sysctl.h>

_Static_assert(PROC_PIDPATHINFO_MAXSIZE == 4096, "update Rust path buffer size");

struct cx_process_snapshot {
    uint64_t start_usec;
    uint32_t pid;
    uint32_t parent;
    uint32_t uid;
    char executable[4096];
    char boot_id[40];
};

int cx_process_snapshot(int pid, struct cx_process_snapshot *out) {
    if (pid <= 0 || out == NULL) {
        errno = EINVAL;
        return -1;
    }
    memset(out, 0, sizeof(*out));
    struct proc_bsdinfo before;
    if (proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &before, sizeof(before)) != sizeof(before)) {
        return -1;
    }
    if (before.pbi_pid != (uint32_t)pid || before.pbi_ppid <= 0 ||
        before.pbi_start_tvsec == 0 || before.pbi_start_tvusec >= 1000000 ||
        before.pbi_status == SZOMB) {
        errno = ESRCH;
        return -1;
    }
    int path_length = proc_pidpath(pid, out->executable, sizeof(out->executable));
    if (path_length <= 0 || memchr(out->executable, '\0', sizeof(out->executable)) == NULL) {
        errno = ESRCH;
        return -1;
    }
    struct proc_bsdinfo after;
    if (proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, &after, sizeof(after)) != sizeof(after) ||
        before.pbi_start_tvsec != after.pbi_start_tvsec ||
        before.pbi_start_tvusec != after.pbi_start_tvusec ||
        before.pbi_pid != after.pbi_pid || before.pbi_ppid != after.pbi_ppid) {
        errno = ESRCH;
        return -1;
    }
    size_t boot_length = sizeof(out->boot_id);
    if (sysctlbyname("kern.bootsessionuuid", out->boot_id, &boot_length, NULL, 0) != 0 ||
        memchr(out->boot_id, '\0', sizeof(out->boot_id)) == NULL) {
        errno = EINVAL;
        return -1;
    }
    out->start_usec = before.pbi_start_tvsec * 1000000 + before.pbi_start_tvusec;
    out->pid = before.pbi_pid;
    out->parent = before.pbi_ppid;
    out->uid = before.pbi_uid;
    return 0;
}
