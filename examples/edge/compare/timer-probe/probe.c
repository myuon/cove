// How late a timed wait wakes on this machine, by wait primitive, interval
// and thread QoS class — the parking lot's question without the server.
//
//   cc -O2 probe.c -o probe && ./probe [reps] [qos,qos...]
//
// For each (QoS, primitive, interval) it waits `reps` times, alone and with
// nothing else to do, and prints the overshoot (wake time - due time)
// percentiles in microseconds. The primitives:
//   cond     pthread_cond_timedwait, which std's `recv_timeout` reaches
//   nanosleep
//   kevent   an EVFILT_TIMER with NOTE_CRITICAL (no coalescing leeway asked)
//   kevent0  an EVFILT_TIMER with no flags
//   kevent-ts  kevent with no filter and a timeout, as Go's netpoller waits
//   mach_wait  mach_wait_until
//   poll     poll(2) on no descriptors with a timeout, as the idle thread waits
#include <errno.h>
#include <poll.h>
#include <mach/mach_time.h>
#include <pthread.h>
#include <pthread/qos.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/qos.h>
#include <time.h>
#include <unistd.h>

static double now_us(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC_RAW, &t);
    return t.tv_sec * 1e6 + t.tv_nsec / 1e3;
}

static void wait_cond(long us) {
    static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
    static pthread_cond_t c = PTHREAD_COND_INITIALIZER;
    struct timespec rel = {us / 1000000, (us % 1000000) * 1000};
    pthread_mutex_lock(&m);
    pthread_cond_timedwait_relative_np(&c, &m, &rel);
    pthread_mutex_unlock(&m);
}

static void wait_nanosleep(long us) {
    struct timespec rel = {us / 1000000, (us % 1000000) * 1000};
    nanosleep(&rel, NULL);
}

static int kq = -1;
static void wait_kevent(long us, int flags) {
    if (kq < 0) kq = kqueue();
    struct kevent ev;
    EV_SET(&ev, 1, EVFILT_TIMER, EV_ADD | EV_ONESHOT, NOTE_USECONDS | flags, us, NULL);
    struct kevent out;
    kevent(kq, &ev, 1, &out, 1, NULL);
}
static void wait_kevent_crit(long us) { wait_kevent(us, NOTE_CRITICAL); }
static void wait_kevent_plain(long us) { wait_kevent(us, 0); }

// kevent with nothing registered and a timeout: what Go's netpoller waits in.
static void wait_kevent_timeout(long us) {
    if (kq < 0) kq = kqueue();
    struct timespec ts = {us / 1000000, (us % 1000000) * 1000};
    struct kevent out;
    kevent(kq, NULL, 0, &out, 1, &ts);
}

static void wait_mach(long us) {
    static mach_timebase_info_data_t tb;
    if (!tb.denom) mach_timebase_info(&tb);
    mach_wait_until(mach_absolute_time() + (uint64_t)us * 1000 * tb.denom / tb.numer);
}

static void wait_poll(long us) { poll(NULL, 0, (int)(us / 1000)); }

static int cmp(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : x > y;
}

struct prim { const char *name; void (*f)(long); };
static const struct prim PRIMS[] = {
    {"cond", wait_cond},
    {"nanosleep", wait_nanosleep},
    {"kevent", wait_kevent_crit},
    {"kevent0", wait_kevent_plain},
    {"kevent-ts", wait_kevent_timeout},
    {"mach_wait", wait_mach},
    {"poll", wait_poll},
};
struct qos { const char *name; qos_class_t q; };
static const struct qos QOS[] = {
    {"inherited", QOS_CLASS_UNSPECIFIED},
    {"default", QOS_CLASS_DEFAULT},
    {"user-initiated", QOS_CLASS_USER_INITIATED},
    {"user-interactive", QOS_CLASS_USER_INTERACTIVE},
    {"utility", QOS_CLASS_UTILITY},
};

int main(int argc, char **argv) {
    int reps = argc > 1 ? atoi(argv[1]) : 20;
    long intervals[] = {1000, 5000, 20000, 60000, 100000};
    qos_class_t start_qos;
    int rel;
    pthread_get_qos_class_np(pthread_self(), &start_qos, &rel);
    printf("# thread QoS at start: 0x%x; reps %d\n", start_qos, reps);
    printf("%-17s %-10s %8s %9s %9s %9s %9s\n", "qos", "primitive", "wait_ms", "p50_us", "p90_us", "max_us", "mean_us");
    double *late = malloc(sizeof(double) * reps);
    const char *only = argc > 2 ? argv[2] : NULL; // a comma list of QoS names
    for (size_t q = 0; q < sizeof QOS / sizeof QOS[0]; q++) {
        if (only && !strstr(only, QOS[q].name)) continue;
        if (QOS[q].q != QOS_CLASS_UNSPECIFIED) pthread_set_qos_class_self_np(QOS[q].q, 0);
        for (size_t p = 0; p < sizeof PRIMS / sizeof PRIMS[0]; p++)
            for (size_t i = 0; i < sizeof intervals / sizeof intervals[0]; i++) {
                double sum = 0;
                for (int r = 0; r < reps; r++) {
                    double t0 = now_us();
                    PRIMS[p].f(intervals[i]);
                    late[r] = now_us() - t0 - intervals[i];
                    sum += late[r];
                }
                qsort(late, reps, sizeof(double), cmp);
                printf("%-17s %-10s %8.0f %9.0f %9.0f %9.0f %9.0f\n", QOS[q].name, PRIMS[p].name, intervals[i] / 1000.0,
                       late[reps / 2], late[reps * 9 / 10], late[reps - 1], sum / reps);
                fflush(stdout);
            }
    }
    return 0;
}
