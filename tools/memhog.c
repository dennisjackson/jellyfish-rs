/* Squeeze the page cache: mmap N GiB of anonymous memory, fault it all in,
 * then keep every page young by re-touching in a slow rolling loop so the
 * kernel prefers evicting file-backed cache over these pages.
 * oom_score_adj is raised so the OOM killer always takes the hog first. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include <fcntl.h>
#include <time.h>

int main(int argc, char **argv) {
    if (argc < 2) { fprintf(stderr, "usage: memhog <GiB>\n"); return 2; }
    size_t gib = strtoul(argv[1], NULL, 10);
    size_t len = gib << 30;

    int fd = open("/proc/self/oom_score_adj", O_WRONLY);
    if (fd >= 0) { (void)!write(fd, "1000", 4); close(fd); }

    char *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    if (p == MAP_FAILED) { perror("mmap"); return 1; }

    /* Fault in, reporting progress every 4 GiB so the caller can watch free(1). */
    for (size_t i = 0; i < len; i += 4096) {
        p[i] = 1;
        if (i && (i % (4UL << 30)) == 0) {
            fprintf(stderr, "memhog: %zu GiB resident\n", i >> 30);
        }
    }
    fprintf(stderr, "memhog: all %zu GiB resident, holding\n", gib);

    /* Rolling re-touch: one full pass roughly every 60 s. */
    size_t pages = len / 4096;
    size_t step = pages / 60 ? pages / 60 : 1;
    for (size_t start = 0;; start = (start + step) % pages) {
        size_t end = start + step < pages ? start + step : pages;
        for (size_t i = start; i < end; i++) p[i * 4096] += 1;
        struct timespec ts = {1, 0};
        nanosleep(&ts, NULL);
    }
}
