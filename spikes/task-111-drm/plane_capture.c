// SPDX-License-Identifier: MIT
//
// TASK-111 spike: can an unprivileged-of-DRM-master, root-owned process read
// the compositor's framebuffer out of a running KMS device?
//
// This is the one question a capture backend stands or falls on, and no
// amount of reading driver source answers it: while GDM's mutter holds DRM
// master on the card, a separate process opens the same card, walks the
// planes, asks for the framebuffer behind the primary plane, turns its GEM
// handle into a dma-buf and maps it for reading. Every step here is a step
// the future VMLord capture service performs on every frame.
//
// What it prints is what the service would need, and what it fails on is
// what would have to be designed around:
//
//   * which plane types the driver exposes, and whether a cursor plane is
//     among them (a driver with no cursor plane means a software cursor
//     composited into the primary framebuffer -- simpler to capture, but
//     the cursor then moves at desktop framerate);
//   * the framebuffer's format, modifier, pitch and size;
//   * the cursor plane's CRTC_X/CRTC_Y, which is what a host renderer draws
//     the pointer at;
//   * how long a full read of the primary framebuffer takes, repeated, which
//     is the ceiling on a CPU capture path before any encoding at all.
//
// Build: cc -O2 -o plane_capture plane_capture.c $(pkg-config --cflags --libs libdrm)
// Run:   sudo ./plane_capture [card] [frames] [dump.ppm]

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#include <linux/dma-buf.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

static double now_ms(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec * 1000.0 + ts.tv_nsec / 1e6;
}

static const char *plane_type_name(int fd, uint32_t plane_id)
{
	drmModeObjectProperties *props =
		drmModeObjectGetProperties(fd, plane_id, DRM_MODE_OBJECT_PLANE);
	const char *name = "unknown";

	if (!props)
		return name;

	for (uint32_t i = 0; i < props->count_props; i++) {
		drmModePropertyRes *p = drmModeGetProperty(fd, props->props[i]);

		if (p && !strcmp(p->name, "type")) {
			switch (props->prop_values[i]) {
			case DRM_PLANE_TYPE_PRIMARY: name = "primary"; break;
			case DRM_PLANE_TYPE_CURSOR:  name = "cursor";  break;
			case DRM_PLANE_TYPE_OVERLAY: name = "overlay"; break;
			}
		}
		if (p)
			drmModeFreeProperty(p);
	}
	drmModeFreeObjectProperties(props);
	return name;
}

// Map the framebuffer behind `fb_id` for reading. Returns the mapping and
// fills `size`; the caller unmaps. Prints the failing step, because a failure
// here is the finding.
static void *map_framebuffer(int fd, uint32_t fb_id, size_t *size,
			     drmModeFB2Ptr *out_fb, int *out_dmabuf)
{
	drmModeFB2Ptr fb = drmModeGetFB2(fd, fb_id);
	int dmabuf = -1;
	void *map;

	if (!fb) {
		printf("      GETFB2 failed: %s\n", strerror(errno));
		return NULL;
	}

	printf("      fb %u: %ux%u fourcc %.4s modifier 0x%llx pitch %u offset %u\n",
	       fb_id, fb->width, fb->height, (const char *)&fb->pixel_format,
	       (unsigned long long)fb->modifier, fb->pitches[0], fb->offsets[0]);

	if (!fb->handles[0]) {
		printf("      no GEM handle returned -- process lacks CAP_SYS_ADMIN "
		       "or the driver withheld it\n");
		drmModeFreeFB2(fb);
		return NULL;
	}

	if (drmPrimeHandleToFD(fd, fb->handles[0], DRM_CLOEXEC | DRM_RDWR, &dmabuf)) {
		printf("      PRIME_HANDLE_TO_FD failed: %s\n", strerror(errno));
		// Retry read-only: some drivers refuse a writable export.
		if (drmPrimeHandleToFD(fd, fb->handles[0], DRM_CLOEXEC, &dmabuf)) {
			printf("      PRIME_HANDLE_TO_FD (read-only) failed too: %s\n",
			       strerror(errno));
			drmModeFreeFB2(fb);
			return NULL;
		}
	}

	*size = (size_t)fb->pitches[0] * fb->height + fb->offsets[0];
	map = mmap(NULL, *size, PROT_READ, MAP_SHARED, dmabuf, 0);
	if (map == MAP_FAILED) {
		printf("      mmap of dma-buf failed: %s\n", strerror(errno));
		printf("      (a driver whose GEM objects are not CPU-mappable "
		       "forces a different capture path)\n");
		close(dmabuf);
		drmModeFreeFB2(fb);
		return NULL;
	}

	*out_fb = fb;
	*out_dmabuf = dmabuf;
	return map;
}

static void write_ppm(const char *path, const uint8_t *pixels, uint32_t width,
		      uint32_t height, uint32_t pitch)
{
	FILE *f = fopen(path, "wb");

	if (!f) {
		printf("      cannot write %s: %s\n", path, strerror(errno));
		return;
	}
	fprintf(f, "P6\n%u %u\n255\n", width, height);
	for (uint32_t y = 0; y < height; y++) {
		const uint8_t *row = pixels + (size_t)y * pitch;

		for (uint32_t x = 0; x < width; x++) {
			// XRGB8888/ARGB8888 little-endian: B,G,R,A in memory.
			const uint8_t *px = row + (size_t)x * 4;
			uint8_t rgb[3] = { px[2], px[1], px[0] };

			fwrite(rgb, 1, 3, f);
		}
	}
	fclose(f);
	printf("      wrote %s\n", path);
}

// One capture tick: bracket the read in dma-buf sync, copy the frame out and
// return a checksum of it. The checksum is the reason the copy survives -O2.
static uint64_t read_frame(int dmabuf, const void *map, uint8_t *sink, size_t size)
{
	struct dma_buf_sync sync = { .flags = DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ };
	uint64_t sum = 0;

	ioctl(dmabuf, DMA_BUF_IOCTL_SYNC, &sync);
	memcpy(sink, map, size);
	sync.flags = DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ;
	ioctl(dmabuf, DMA_BUF_IOCTL_SYNC, &sync);

	for (size_t i = 0; i < size; i += 8)
		sum += sink[i] * (uint64_t)(i + 1);
	return sum;
}

// Say whether there is a picture here at all. A capture that succeeds against
// a framebuffer nobody rendered into looks identical, in every log line, to a
// capture of a working desktop -- until someone looks at the pixels.
static void describe_frame(const uint8_t *pixels, uint32_t width, uint32_t height,
			   uint32_t pitch)
{
	uint32_t first = *(const uint32_t *)pixels;
	size_t differing = 0;

	for (uint32_t y = 0; y < height; y++) {
		const uint32_t *row = (const uint32_t *)(pixels + (size_t)y * pitch);

		for (uint32_t x = 0; x < width; x++)
			if (row[x] != first)
				differing++;
	}

	printf("frame content: %s (first pixel 0x%08x, %zu of %u pixels differ from it)\n",
	       differing == 0 ? "ONE FLAT COLOUR -- nothing rendered into it"
			      : "a picture",
	       first, differing, width * height);
}

int main(int argc, char **argv)
{
	const char *card = argc > 1 ? argv[1] : "/dev/dri/card0";
	int frames = argc > 2 ? atoi(argv[2]) : 30;
	const char *dump = argc > 3 ? argv[3] : "/tmp/plane_capture.ppm";
	drmModePlaneRes *planes;
	uint32_t primary_fb = 0;
	int fd;

	fd = open(card, O_RDWR | O_CLOEXEC);
	if (fd < 0) {
		fprintf(stderr, "open %s: %s\n", card, strerror(errno));
		return 1;
	}

	printf("card: %s\n", card);
	printf("master held by this process: %s\n",
	       drmIsMaster(fd) ? "yes (nothing else has the card)"
			       : "no (a compositor holds it -- the interesting case)");

	if (drmSetClientCap(fd, DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1))
		printf("UNIVERSAL_PLANES rejected: %s\n", strerror(errno));

	planes = drmModeGetPlaneResources(fd);
	if (!planes) {
		fprintf(stderr, "GETPLANERESOURCES: %s\n", strerror(errno));
		close(fd);
		return 1;
	}

	printf("planes: %u\n", planes->count_planes);
	for (uint32_t i = 0; i < planes->count_planes; i++) {
		drmModePlane *plane = drmModeGetPlane(fd, planes->planes[i]);
		const char *type;

		if (!plane)
			continue;

		type = plane_type_name(fd, planes->planes[i]);
		printf("  plane %u (%s): crtc %u fb %u at %d,%d\n",
		       plane->plane_id, type, plane->crtc_id, plane->fb_id,
		       plane->crtc_x, plane->crtc_y);

		if (plane->fb_id) {
			size_t size = 0;
			drmModeFB2Ptr fb = NULL;
			int dmabuf = -1;
			void *map = map_framebuffer(fd, plane->fb_id, &size, &fb, &dmabuf);

			if (map) {
				printf("      mapped %zu bytes for reading\n", size);
				if (!strcmp(type, "primary")) {
					primary_fb = plane->fb_id;
					write_ppm(dump, map, fb->width, fb->height,
						  fb->pitches[0]);
				}
				munmap(map, size);
				close(dmabuf);
			}
			if (fb)
				drmModeFreeFB2(fb);
		} else {
			printf("      no framebuffer attached\n");
		}
		drmModeFreePlane(plane);
	}

	// Timed re-reads of the primary framebuffer: the cost of a capture
	// tick, with no encoding and no transport in it. The checksum is not
	// decoration -- it is what keeps the compiler from deleting the copy
	// it can otherwise prove nobody reads.
	if (primary_fb && frames > 0) {
		size_t size = 0;
		drmModeFB2Ptr fb = NULL;
		int dmabuf = -1;
		void *map = map_framebuffer(fd, primary_fb, &size, &fb, &dmabuf);

		if (map) {
			uint8_t *sink = malloc(size);

			if (sink) {
				uint64_t sum = 0;
				double start = now_ms(), total;

				for (int i = 0; i < frames; i++) {
					sum += read_frame(dmabuf, map, sink, size);
				}
				total = now_ms() - start;
				printf("\ncopy of %zu bytes x%d: %.1f ms total, "
				       "%.2f ms per frame (%.0f fps ceiling, copy only)\n",
				       size, frames, total, total / frames,
				       1000.0 / (total / frames));
				printf("checksum across the run: 0x%llx\n",
				       (unsigned long long)sum);

				// A framebuffer that reads as one flat colour is a
				// framebuffer nothing rendered into: the capture
				// path worked and there was nothing behind it.
				describe_frame(sink, fb->width, fb->height, fb->pitches[0]);

				// And one that never changes is a compositor that
				// is not drawing, however alive its process list
				// looks. One second is long enough for a greeter
				// with a clock on it.
				{
					uint64_t before = read_frame(dmabuf, map, sink, size);
					uint64_t after;

					sleep(1);
					after = read_frame(dmabuf, map, sink, size);
					printf("content over one second: %s\n",
					       before == after
						       ? "unchanged (nothing is drawing)"
						       : "changed (the compositor is drawing)");
				}
				free(sink);
			}
			munmap(map, size);
			close(dmabuf);
		}
		if (fb)
			drmModeFreeFB2(fb);
	}

	drmModeFreePlaneResources(planes);
	close(fd);
	return 0;
}
