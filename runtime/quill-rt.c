/* Runtime helpers for the Quill native compiler.
 *
 * These are plain C because they need variadic libc calls (snprintf), which
 * cannot be driven through Cranelift's fixed-ABI call convention: variadic
 * arguments travel on the stack, not in registers, so a Cranelift call would
 * hand snprintf garbage. Everything here returns a malloc'd, NUL-terminated
 * string; the caller boxes it and frees it.
 */

#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <unistd.h>

/* Decimal representation of a boxed Int payload. */
char *quill_itoa(long n) {
    char buf[32];
    int len = snprintf(buf, sizeof buf, "%lld", (long long)n);
    char *out = (char *)malloc((size_t)len + 1);
    if (out == NULL) {
        return NULL;
    }
    memcpy(out, buf, (size_t)len + 1);
    return out;
}

/* Float formatting that matches the interpreter's `format_value`: whole
 * values get one decimal place ("3.0"); anything else uses the shortest
 * `%g`-style representation that round-trips ("12.56636"). The argument is
 * the f64 bit pattern, not a real double, so the call stays all-integer. */
char *quill_ftoa(long bits) {
    double f;
    memcpy(&f, &bits, sizeof f);
    char buf[64];
    int len;
    if (isfinite(f) && f == trunc(f)) {
        len = snprintf(buf, sizeof buf, "%.1f", f);
    } else {
        /* Find the shortest representation that parses back to `f` exactly. */
        int best = 0;
        for (int p = 1; p <= 17; p++) {
            int n = snprintf(buf, sizeof buf, "%.*g", p, f);
            if (n <= 0) {
                continue;
            }
            char *end = NULL;
            double back = strtod(buf, &end);
            if (end != buf && back == f) {
                best = n;
                break;
            }
        }
        if (best == 0) {
            len = snprintf(buf, sizeof buf, "%g", f);
        } else {
            len = best;
        }
    }
    char *out = (char *)malloc((size_t)len + 1);
    if (out == NULL) {
        return NULL;
    }
    memcpy(out, buf, (size_t)len + 1);
    return out;
}

/* Read one line of stdin without its trailing newline/CR; NULL at end of
 * input. The returned buffer is malloc'd; the caller boxes it and frees it.
 * The length (which can include embedded NUL bytes) is stashed in
 * `quill_line_len`, because the caller cannot recover it with strlen. */
static ssize_t quill_line_len = 0;

char *quill_read_line_raw(void) {
    char *line = NULL;
    size_t cap = 0;
    ssize_t n = getline(&line, &cap, stdin);
    if (n < 0) {
        free(line);
        quill_line_len = 0;
        return NULL;
    }
    while (n > 0 && (line[n - 1] == '\n' || line[n - 1] == '\r')) {
        line[--n] = '\0';
    }
    quill_line_len = n;
    return line;
}

/* The length of the line returned by the most recent quill_read_line_raw
 * call (0 at end of input). */
long quill_read_line_raw_len(void) { return (long)quill_line_len; }
