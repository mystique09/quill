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

/* Read one line of stdin without its trailing newline/CR. Returns NULL at
 * end of input and (char *)-1 on a read error or invalid UTF-8 (so the
 * caller can trap, mirroring the interpreter's readLine errors). The
 * returned buffer is malloc'd; the caller boxes it and frees it. The length
 * (which can include embedded NUL bytes) is stashed in `quill_line_len`,
 * because the caller cannot recover it with strlen. */
static ssize_t quill_line_len = 0;

/* Is `line[0..n]` valid UTF-8 (rejecting overlong encodings, surrogates,
 * and code points above U+10FFFF, like Rust's String)? */
static int quill_utf8_valid(const char *line, ssize_t n) {
    ssize_t i = 0;
    while (i < n) {
        unsigned char c = (unsigned char)line[i];
        if (c < 0x80) {
            i++;
            continue;
        }
        int cont;
        unsigned long cp;
        unsigned long min;
        if ((c & 0xE0) == 0xC0) {
            cont = 1;
            cp = c & 0x1F;
            min = 0x80;
        } else if ((c & 0xF0) == 0xE0) {
            cont = 2;
            cp = c & 0x0F;
            min = 0x800;
        } else if ((c & 0xF8) == 0xF0) {
            cont = 3;
            cp = c & 0x07;
            min = 0x10000;
        } else {
            return 0;
        }
        if (i + cont + 1 > n) {
            return 0; /* truncated sequence */
        }
        for (int k = 1; k <= cont; k++) {
            unsigned char cc = (unsigned char)line[i + k];
            if ((cc & 0xC0) != 0x80) {
                return 0;
            }
            cp = (cp << 6) | (cc & 0x3F);
        }
        if (cp < min || cp > 0x10FFFF || (cp >= 0xD800 && cp <= 0xDFFF)) {
            return 0;
        }
        i += cont + 1;
    }
    return 1;
}

char *quill_read_line_raw(void) {
    char *line = NULL;
    size_t cap = 0;
    ssize_t n = getline(&line, &cap, stdin);
    if (n < 0) {
        free(line);
        quill_line_len = 0;
        return ferror(stdin) ? (char *)-1 : NULL;
    }
    while (n > 0 && (line[n - 1] == '\n' || line[n - 1] == '\r')) {
        line[--n] = '\0';
    }
    if (!quill_utf8_valid(line, n)) {
        free(line);
        quill_line_len = 0;
        return (char *)-1;
    }
    quill_line_len = n;
    return line;
}

/* The length of the line returned by the most recent quill_read_line_raw
 * call (0 at end of input). */
long quill_read_line_raw_len(void) { return (long)quill_line_len; }
