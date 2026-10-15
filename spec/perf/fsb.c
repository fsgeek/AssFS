/* fsb: append 4 KiB + fsync, N times; print ops/s and latency percentiles (us). */
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }
static int cmp(const void *a, const void *b) { double x = *(double *)a, y = *(double *)b; return (x > y) - (x < y); }
int main(int argc, char **argv)
{
	int n = atoi(argv[2]), fd = open(argv[1], O_CREAT | O_TRUNC | O_WRONLY, 0644);
	char buf[4096]; double *lat = malloc(n * sizeof *lat), t0, t;
	memset(buf, 'x', sizeof buf);
	if (fd < 0) { perror("open"); return 1; }
	t0 = now();
	for (int i = 0; i < n; i++) {
		t = now();
		if (write(fd, buf, sizeof buf) != sizeof buf || fsync(fd)) { perror("io"); return 1; }
		lat[i] = (now() - t) * 1e6;
	}
	t = now() - t0;
	qsort(lat, n, sizeof *lat, cmp);
	printf("RESULT n=%d ops/s=%.1f p50=%.0f p90=%.0f p99=%.0f us\n", n, n / t, lat[n / 2], lat[n * 9 / 10], lat[n * 99 / 100]);
	return 0;
}
