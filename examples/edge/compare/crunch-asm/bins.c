// Times machine code the native tier emitted (COVE_NATIVE_DUMP) for
// crunch's primesUpTo, against Go's loop, in interleaved rounds.
//
//   cc -O2 bins.c go.s -o bins && OFFSETS=0,16,32,48 ./bins n rounds min_ms a.bin b.bin ...
//
// OFFSETS places each .bin that many bytes past a page boundary (default 0,
// which is where the tier's own mapping puts a function), so a difference
// that is only code alignment shows up as a spread across offsets.
//
// Each .bin is entered the way the runtime enters it (crate::abi's Entry:
// ctx, base, return_base, return_slot) over a context that holds only what
// primesUpTo's code reads: the segment pointer at +0x8 and a poll threshold
// at +0x70 that is never reached. A function whose code calls a helper or
// raises is not one this harness can run, and answers -1.
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

// go.s: Go's loop, and the same with a 32-bit division.
long V_go(long n, long *unused);
long V_go_div32(long n, long *unused);
static long (*const GO[2])(long, long *) = {V_go, V_go_div32};
static const char *const GO_NAME[2] = {"go (go.s)", "go, idivl (go.s)"};

typedef int (*entry_t)(long *ctx, unsigned long base, unsigned long return_base, unsigned return_slot);

static long *ctx, *words;

static double now_ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC_RAW, &t);
    return t.tv_sec * 1e9 + t.tv_nsec;
}

static entry_t load(const char *path, int offset) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) { perror(path); exit(1); }
    struct stat st;
    fstat(fd, &st);
    size_t len = (st.st_size + offset + 4095) & ~4095UL;
    void *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    memset(p, 0xcc, offset);
    if (read(fd, (char *)p + offset, st.st_size) != st.st_size) { perror("read"); exit(1); }
    close(fd);
    mprotect(p, len, PROT_READ | PROT_EXEC);
    return (entry_t)((char *)p + offset);
}

static long run_bin(entry_t f, long n) {
    words[0] = n;
    int outcome = f(ctx, 0, 32, 0);
    return outcome == 0 ? words[32] : -1;
}

static int cmp(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return x < y ? -1 : x > y;
}

int main(int argc, char **argv) {
    if (argc < 5) { fprintf(stderr, "usage: bins n rounds min_ms a.bin ...\n"); return 2; }
    long n = atol(argv[1]);
    int rounds = atoi(argv[2]);
    double min_ns = atof(argv[3]) * 1e6;
    const char *offs_env = getenv("OFFSETS");
    int offs[16], no = 0;
    char buf[256];
    snprintf(buf, sizeof buf, "%s", offs_env ? offs_env : "0");
    for (char *t = strtok(buf, ","); t && no < 16; t = strtok(NULL, ",")) offs[no++] = atoi(t);
    int nfiles = argc - 4;
    int nb = nfiles * no;
    int nv = nb + 2; // the bins at each offset, then the two Go loops
    char **names = calloc(nb, sizeof *names);
    ctx = calloc(64, sizeof(long));
    words = calloc(64, sizeof(long));
    ctx[1] = (long)words;
    ctx[14] = -1;
    long gctx[32] = {0};
    entry_t *f = calloc(nb, sizeof *f);
    for (int i = 0; i < nfiles; i++)
        for (int o = 0; o < no; o++) {
            f[i * no + o] = load(argv[4 + i], offs[o]);
            names[i * no + o] = malloc(300);
            snprintf(names[i * no + o], 300, "%s@%d", argv[4 + i], offs[o]);
        }
    long expect = V_go(n, gctx);
    for (int i = 0; i < nb; i++) {
        long got = run_bin(f[i], n);
        if (i == 0 && V_go_div32(n, gctx) != expect) { fprintf(stderr, "go_div32 disagrees\n"); return 1; }
        if (got != expect) { fprintf(stderr, "%s answered %ld, go %ld\n", names[i], got, expect); return 1; }
    }
    long calls = 1;
    for (;;) {
        double t0 = now_ns();
        for (long i = 0; i < calls; i++) run_bin(f[0], n);
        if (now_ns() - t0 > min_ns) break;
        calls *= 2;
    }
    double (*per)[256] = calloc(nv, sizeof *per);
    for (int r = 0; r < rounds; r++)
        for (int k = 0; k < nv; k++) {
            int v = (k + r) % nv;
            volatile long sink = 0;
            double t0 = now_ns();
            if (v < nb)
                for (long i = 0; i < calls; i++) sink += run_bin(f[v], n);
            else
                for (long i = 0; i < calls; i++) sink += GO[v - nb](n, gctx);
            per[v][r] = (now_ns() - t0) / calls;
        }
    printf("# n=%ld answer=%ld calls/batch=%ld rounds=%d\n", n, expect, calls, rounds);
    printf("%-40s %12s %10s %10s\n", "code@offset", "median ns", "min", "max");
    double *med = calloc(nv, sizeof *med);
    for (int v = 0; v < nv; v++) {
        qsort(per[v], rounds, sizeof(double), cmp);
        med[v] = per[v][rounds / 2];
        printf("%-40s %12.0f %10.0f %10.0f\n", v < nb ? names[v] : GO_NAME[v - nb], med[v], per[v][0], per[v][rounds - 1]);
    }
    if (no > 1) {
        printf("\n# over offsets: mean of the per-offset medians, and their range\n");
        printf("%-40s %12s %10s %10s %8s\n", "code", "mean ns", "min", "max", "vs first");
        double first = 0;
        for (int i = 0; i < nfiles; i++) {
            double sum = 0, lo = 1e18, hi = 0;
            for (int o = 0; o < no; o++) {
                double m = med[i * no + o];
                sum += m; if (m < lo) lo = m; if (m > hi) hi = m;
            }
            double mean = sum / no;
            if (i == 0) first = mean;
            printf("%-40s %12.0f %10.0f %10.0f %+7.1f%%\n", argv[4 + i], mean, lo, hi, 100 * (mean / first - 1));
        }
        for (int g = 0; g < 2; g++)
            printf("%-40s %12.0f %+7.1f%%\n", GO_NAME[g], med[nb + g], 100 * (med[nb + g] / first - 1));
    }
    return 0;
}
