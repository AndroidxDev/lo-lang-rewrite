#ifndef LO_RUNTIME_H
#define LO_RUNTIME_H

#define _POSIX_C_SOURCE 200809L
#include <ctype.h>
#include <errno.h>
#include <inttypes.h>
#include <math.h>
#include <pthread.h>
#include <setjmp.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

typedef struct {
    uint8_t _unused;
} lo_unit_t;

static inline lo_unit_t lo_unit_val(void) {
    lo_unit_t u = {0};
    return u;
}

/* =========================================================================
 * 1. Perceus Atomic Reference Counting & In-Place Reuse Allocator
 * ========================================================================= */

typedef struct {
    _Atomic uint32_t rc;
    uint32_t cap_bytes;
    void (*drop_fn)(void*);
} lo_rc_hdr_t;

static _Atomic int64_t _lo_perceus_allocs = 0;
static _Atomic int64_t _lo_perceus_reuses = 0;

static inline lo_rc_hdr_t* lo_rc_hdr(void* ptr) {
    return ((lo_rc_hdr_t*)ptr) - 1;
}

static inline void* lo_rc_alloc(size_t payload_bytes, void (*drop_fn)(void*)) {
    size_t cap = payload_bytes < 16 ? 16 : payload_bytes;
    lo_rc_hdr_t* hdr = (lo_rc_hdr_t*)calloc(1, sizeof(lo_rc_hdr_t) + cap);
    if (!hdr) {
        fprintf(stderr, "lo panic: out of memory allocating %zu bytes\n", payload_bytes);
        exit(1);
    }
    atomic_init(&hdr->rc, 1);
    hdr->cap_bytes = (uint32_t)cap;
    hdr->drop_fn = drop_fn;
    atomic_fetch_add(&_lo_perceus_allocs, 1);
    return (void*)(hdr + 1);
}

static inline void* lo_rc_dup(void* ptr) {
    if (ptr) {
        lo_rc_hdr_t* hdr = lo_rc_hdr(ptr);
        atomic_fetch_add_explicit(&hdr->rc, 1, memory_order_relaxed);
    }
    return ptr;
}

static inline bool lo_rc_is_unique(void* ptr) {
    if (!ptr) return false;
    return atomic_load_explicit(&lo_rc_hdr(ptr)->rc, memory_order_acquire) == 1;
}

static inline void lo_rc_drop(void* ptr) {
    if (!ptr) return;
    lo_rc_hdr_t* hdr = lo_rc_hdr(ptr);
    if (atomic_fetch_sub_explicit(&hdr->rc, 1, memory_order_acq_rel) == 1) {
        if (hdr->drop_fn) {
            hdr->drop_fn(ptr);
        }
        free(hdr);
    }
}

static inline void* lo_rc_drop_reuse(void* ptr, size_t payload_bytes, void (*drop_fn)(void*)) {
    if (ptr) {
        lo_rc_hdr_t* hdr = lo_rc_hdr(ptr);
        if (atomic_load_explicit(&hdr->rc, memory_order_acquire) == 1 &&
            hdr->cap_bytes >= payload_bytes) {
            if (hdr->drop_fn) {
                hdr->drop_fn(ptr);
            }
            memset(ptr, 0, payload_bytes);
            hdr->drop_fn = drop_fn;
            atomic_fetch_add(&_lo_perceus_reuses, 1);
            return ptr;
        }
        lo_rc_drop(ptr);
    }
    return lo_rc_alloc(payload_bytes, drop_fn);
}

static inline int64_t lo_perceus_reuse_count(void) {
    return atomic_load(&_lo_perceus_reuses);
}

static inline void lo_panic(const char* msg, int line) {
    fprintf(stderr, "lo panic (line %d): %s\n", line, msg);
    exit(1);
}

/* =========================================================================
 * 2. UTF-8 Strings (`lo_str`)
 * ========================================================================= */

typedef struct {
    const char* data;
    int64_t len;
    void* rc_buf;
} lo_str;

static inline lo_str lo_str_lit(const char* cstr, int64_t len) {
    lo_str s = { cstr, len, NULL };
    return s;
}

static inline lo_str lo_str_owned_copy(const char* src, int64_t len) {
    if (len <= 0) {
        return lo_str_lit("", 0);
    }
    char* buf = (char*)lo_rc_alloc((size_t)len + 1, NULL);
    memcpy(buf, src, (size_t)len);
    buf[len] = '\0';
    lo_str s = { buf, len, buf };
    return s;
}

static inline lo_str lo_str_from_cstr(const char* cstr) {
    if (!cstr) return lo_str_lit("", 0);
    return lo_str_owned_copy(cstr, (int64_t)strlen(cstr));
}

static inline const char* lo_cstr(lo_str s) {
    return s.data ? s.data : "";
}

static inline bool lo_str_eq(lo_str a, lo_str b) {
    if (a.len != b.len) return false;
    if (a.len == 0) return true;
    return memcmp(a.data, b.data, (size_t)a.len) == 0;
}

static inline int lo_str_cmp(lo_str a, lo_str b) {
    int64_t min_len = a.len < b.len ? a.len : b.len;
    int cmp = min_len > 0 ? memcmp(a.data, b.data, (size_t)min_len) : 0;
    if (cmp != 0) return cmp;
    return (a.len > b.len) - (a.len < b.len);
}

static inline lo_str lo_str_concat(lo_str a, lo_str b) {
    if (a.len == 0) { lo_rc_dup(b.rc_buf); return b; }
    if (b.len == 0) { lo_rc_dup(a.rc_buf); return a; }
    int64_t total = a.len + b.len;
    char* buf = (char*)lo_rc_alloc((size_t)total + 1, NULL);
    memcpy(buf, a.data, (size_t)a.len);
    memcpy(buf + a.len, b.data, (size_t)b.len);
    buf[total] = '\0';
    lo_str res = { buf, total, buf };
    return res;
}

static inline lo_str lo_str_join_parts(const lo_str* parts, size_t count) {
    int64_t total = 0;
    for (size_t i = 0; i < count; i++) total += parts[i].len;
    if (total == 0) return lo_str_lit("", 0);
    char* buf = (char*)lo_rc_alloc((size_t)total + 1, NULL);
    int64_t off = 0;
    for (size_t i = 0; i < count; i++) {
        if (parts[i].len > 0) {
            memcpy(buf + off, parts[i].data, (size_t)parts[i].len);
            off += parts[i].len;
        }
    }
    buf[total] = '\0';
    lo_str res = { buf, total, buf };
    return res;
}

static inline lo_str lo_fmt_i64(int64_t v) {
    char tmp[64];
    int n = snprintf(tmp, sizeof(tmp), "%" PRId64, v);
    return lo_str_owned_copy(tmp, n);
}

static inline lo_str lo_fmt_u64(uint64_t v) {
    char tmp[64];
    int n = snprintf(tmp, sizeof(tmp), "%" PRIu64, v);
    return lo_str_owned_copy(tmp, n);
}

static inline lo_str lo_fmt_f64(double v) {
    char tmp[64];
    int n = snprintf(tmp, sizeof(tmp), "%g", v);
    return lo_str_owned_copy(tmp, n);
}

static inline lo_str lo_fmt_bool(bool v) {
    return v ? lo_str_lit("1b", 2) : lo_str_lit("0b", 2);
}

static inline lo_str lo_fmt_char(uint32_t cp) {
    char utf8[5] = {0};
    int len = 0;
    if (cp <= 0x7F) {
        utf8[0] = (char)cp;
        len = 1;
    } else if (cp <= 0x7FF) {
        utf8[0] = (char)(0xC0 | (cp >> 6));
        utf8[1] = (char)(0x80 | (cp & 0x3F));
        len = 2;
    } else if (cp <= 0xFFFF) {
        utf8[0] = (char)(0xE0 | (cp >> 12));
        utf8[1] = (char)(0x80 | ((cp >> 6) & 0x3F));
        utf8[2] = (char)(0x80 | (cp & 0x3F));
        len = 3;
    } else {
        utf8[0] = (char)(0xF0 | (cp >> 18));
        utf8[1] = (char)(0x80 | ((cp >> 12) & 0x3F));
        utf8[2] = (char)(0x80 | ((cp >> 6) & 0x3F));
        utf8[3] = (char)(0x80 | (cp & 0x3F));
        len = 4;
    }
    return lo_str_owned_copy(utf8, len);
}

static inline uint32_t lo_str_char_at(lo_str s, int64_t idx, int line) {
    if (idx < 0 || idx >= s.len) {
        lo_panic("string index out of bounds", line);
    }
    return (uint8_t)s.data[idx];
}

static inline lo_str lo_str_slice(lo_str s, int64_t start, int64_t end, bool inclusive) {
    int64_t real_end = inclusive ? (end + 1) : end;
    if (start < 0) start = 0;
    if (real_end > s.len) real_end = s.len;
    if (start >= real_end) return lo_str_lit("", 0);
    return lo_str_owned_copy(s.data + start, real_end - start);
}

static inline lo_str lo_str_trim(lo_str s) {
    int64_t a = 0;
    int64_t b = s.len;
    while (a < b && isspace((unsigned char)s.data[a])) a++;
    while (b > a && isspace((unsigned char)s.data[b - 1])) b--;
    return lo_str_owned_copy(s.data + a, b - a);
}

static inline lo_str lo_str_upper(lo_str s) {
    if (s.len == 0) return lo_str_lit("", 0);
    char* buf = (char*)lo_rc_alloc((size_t)s.len + 1, NULL);
    for (int64_t i = 0; i < s.len; i++) {
        buf[i] = (char)toupper((unsigned char)s.data[i]);
    }
    buf[s.len] = '\0';
    lo_str res = { buf, s.len, buf };
    return res;
}

static inline lo_str lo_str_lower(lo_str s) {
    if (s.len == 0) return lo_str_lit("", 0);
    char* buf = (char*)lo_rc_alloc((size_t)s.len + 1, NULL);
    for (int64_t i = 0; i < s.len; i++) {
        buf[i] = (char)tolower((unsigned char)s.data[i]);
    }
    buf[s.len] = '\0';
    lo_str res = { buf, s.len, buf };
    return res;
}

static inline bool lo_str_contains(lo_str s, lo_str sub) {
    if (sub.len == 0) return true;
    if (sub.len > s.len) return false;
    for (int64_t i = 0; i <= s.len - sub.len; i++) {
        if (memcmp(s.data + i, sub.data, (size_t)sub.len) == 0) return true;
    }
    return false;
}

static inline bool lo_str_starts_with(lo_str s, lo_str prefix) {
    if (prefix.len > s.len) return false;
    return prefix.len == 0 || memcmp(s.data, prefix.data, (size_t)prefix.len) == 0;
}

static inline bool lo_str_ends_with(lo_str s, lo_str suffix) {
    if (suffix.len > s.len) return false;
    return suffix.len == 0 ||
           memcmp(s.data + (s.len - suffix.len), suffix.data, (size_t)suffix.len) == 0;
}

static inline lo_str lo_str_replace(lo_str s, lo_str from, lo_str to) {
    if (from.len == 0 || s.len < from.len) return s;
    size_t cap = (size_t)(s.len * 2 + to.len + 16);
    char* out = (char*)malloc(cap);
    size_t out_len = 0;
    int64_t i = 0;
    while (i < s.len) {
        if (i <= s.len - from.len &&
            memcmp(s.data + i, from.data, (size_t)from.len) == 0) {
            if (out_len + (size_t)to.len + 1 > cap) {
                cap = (out_len + (size_t)to.len + 1) * 2;
                out = (char*)realloc(out, cap);
            }
            if (to.len > 0) memcpy(out + out_len, to.data, (size_t)to.len);
            out_len += (size_t)to.len;
            i += from.len;
        } else {
            if (out_len + 2 > cap) {
                cap *= 2;
                out = (char*)realloc(out, cap);
            }
            out[out_len++] = s.data[i++];
        }
    }
    lo_str res = lo_str_owned_copy(out, (int64_t)out_len);
    free(out);
    return res;
}

static inline bool lo_str_parse_i64(lo_str s, int64_t* out_val) {
    lo_str t = lo_str_trim(s);
    if (t.len == 0) return false;
    char* endp = NULL;
    errno = 0;
    long long v = strtoll(lo_cstr(t), &endp, 10);
    if (errno != 0 || endp != lo_cstr(t) + t.len) {
        lo_rc_drop(t.rc_buf);
        return false;
    }
    lo_rc_drop(t.rc_buf);
    *out_val = (int64_t)v;
    return true;
}

static inline bool lo_str_parse_f64(lo_str s, double* out_val) {
    lo_str t = lo_str_trim(s);
    if (t.len == 0) return false;
    char* endp = NULL;
    errno = 0;
    double v = strtod(lo_cstr(t), &endp);
    if (errno != 0 || endp != lo_cstr(t) + t.len) {
        lo_rc_drop(t.rc_buf);
        return false;
    }
    lo_rc_drop(t.rc_buf);
    *out_val = v;
    return true;
}

/* =========================================================================
 * 3. Dynamic Arrays & Zero-Copy Slices (`lo_vec`)
 * ========================================================================= */

typedef struct lo_vec {
    uint8_t* data;
    int64_t len;
    int64_t cap;
    size_t elem_size;
    void* backing_owner;
} lo_vec;

static void lo_vec_drop_fn(void* p) {
    lo_vec* v = (lo_vec*)p;
    if (v->backing_owner) {
        lo_rc_drop(v->backing_owner);
    } else if (v->data) {
        free(v->data);
    }
}

static inline lo_vec* lo_vec_new(size_t elem_size, int64_t init_cap) {
    lo_vec* v = (lo_vec*)lo_rc_alloc(sizeof(lo_vec), lo_vec_drop_fn);
    v->elem_size = elem_size > 0 ? elem_size : 1;
    v->len = 0;
    v->cap = init_cap > 0 ? init_cap : 4;
    v->data = (uint8_t*)calloc((size_t)v->cap, v->elem_size);
    v->backing_owner = NULL;
    return v;
}

static inline lo_vec* lo_vec_repeat(size_t elem_size, const void* elem_ptr, int64_t count) {
    if (count < 0) count = 0;
    lo_vec* v = lo_vec_new(elem_size, count > 0 ? count : 1);
    v->len = count;
    for (int64_t i = 0; i < count; i++) {
        memcpy(v->data + (size_t)i * v->elem_size, elem_ptr, v->elem_size);
    }
    return v;
}

static inline void lo_vec_ensure_writable(lo_vec* v, int64_t min_cap) {
    if (v->backing_owner != NULL) {
        int64_t new_cap = min_cap > v->len * 2 ? min_cap : (v->len * 2 + 4);
        uint8_t* fresh = (uint8_t*)calloc((size_t)new_cap, v->elem_size);
        if (v->len > 0) {
            memcpy(fresh, v->data, (size_t)v->len * v->elem_size);
        }
        lo_rc_drop(v->backing_owner);
        v->backing_owner = NULL;
        v->data = fresh;
        v->cap = new_cap;
    } else if (min_cap > v->cap) {
        int64_t new_cap = min_cap > v->cap * 2 ? min_cap : (v->cap * 2 + 4);
        v->data = (uint8_t*)realloc(v->data, (size_t)new_cap * v->elem_size);
        v->cap = new_cap;
    }
}

static inline void lo_vec_push(lo_vec* v, const void* elem_ptr) {
    lo_vec_ensure_writable(v, v->len + 1);
    memcpy(v->data + (size_t)v->len * v->elem_size, elem_ptr, v->elem_size);
    v->len++;
}

static inline bool lo_vec_pop(lo_vec* v, void* out_elem) {
    if (!v || v->len == 0) return false;
    lo_vec_ensure_writable(v, v->len);
    v->len--;
    memcpy(out_elem, v->data + (size_t)v->len * v->elem_size, v->elem_size);
    return true;
}

static inline void* lo_vec_at(lo_vec* v, int64_t idx, int line) {
    if (!v || idx < 0 || idx >= v->len) {
        lo_panic("array index out of bounds", line);
    }
    return v->data + (size_t)idx * v->elem_size;
}

/* Zero-copy slice sharing Perceus RC backing buffer (`lo.md` §5.2) */
static inline lo_vec* lo_vec_slice(lo_vec* src, int64_t start, int64_t end, bool inclusive) {
    if (!src) return lo_vec_new(1, 0);
    int64_t real_end = inclusive ? (end + 1) : end;
    if (start < 0) start = 0;
    if (real_end > src->len) real_end = src->len;
    if (start >= real_end) return lo_vec_new(src->elem_size, 0);
    lo_vec* view = (lo_vec*)lo_rc_alloc(sizeof(lo_vec), lo_vec_drop_fn);
    view->elem_size = src->elem_size;
    view->len = real_end - start;
    view->cap = view->len;
    view->data = src->data + (size_t)start * src->elem_size;
    void* root = src->backing_owner ? src->backing_owner : (void*)src;
    view->backing_owner = lo_rc_dup(root);
    return view;
}

static inline lo_vec* lo_str_split(lo_str s, lo_str sep) {
    lo_vec* out = lo_vec_new(sizeof(lo_str), 4);
    if (sep.len == 0) {
        for (int64_t i = 0; i < s.len; i++) {
            lo_str ch = lo_str_owned_copy(s.data + i, 1);
            lo_vec_push(out, &ch);
        }
        return out;
    }
    int64_t start = 0;
    int64_t i = 0;
    while (i <= s.len - sep.len) {
        if (memcmp(s.data + i, sep.data, (size_t)sep.len) == 0) {
            lo_str part = lo_str_owned_copy(s.data + start, i - start);
            lo_vec_push(out, &part);
            i += sep.len;
            start = i;
        } else {
            i++;
        }
    }
    lo_str tail = lo_str_owned_copy(s.data + start, s.len - start);
    lo_vec_push(out, &tail);
    return out;
}

static inline lo_str lo_str_join_vec(lo_vec* parts, lo_str sep) {
    if (!parts || parts->len == 0) return lo_str_lit("", 0);
    int64_t total = 0;
    for (int64_t i = 0; i < parts->len; i++) {
        lo_str* p = (lo_str*)(parts->data + (size_t)i * sizeof(lo_str));
        total += p->len;
        if (i + 1 < parts->len) total += sep.len;
    }
    if (total == 0) return lo_str_lit("", 0);
    char* buf = (char*)lo_rc_alloc((size_t)total + 1, NULL);
    int64_t off = 0;
    for (int64_t i = 0; i < parts->len; i++) {
        lo_str* p = (lo_str*)(parts->data + (size_t)i * sizeof(lo_str));
        if (p->len > 0) {
            memcpy(buf + off, p->data, (size_t)p->len);
            off += p->len;
        }
        if (i + 1 < parts->len && sep.len > 0) {
            memcpy(buf + off, sep.data, (size_t)sep.len);
            off += sep.len;
        }
    }
    buf[total] = '\0';
    lo_str res = { buf, total, buf };
    return res;
}

/* =========================================================================
 * 4. HashMap (`lo_map`) & HashSet (`lo_set`)
 * ========================================================================= */

typedef struct {
    bool occupied;
    uint64_t hash;
    void* key;
    void* val;
} lo_map_entry_t;

typedef struct lo_map {
    lo_map_entry_t* entries;
    size_t cap;
    int64_t len;
    size_t key_size;
    size_t val_size;
    bool is_str_key;
} lo_map;

static uint64_t lo_hash_bytes(const uint8_t* bytes, size_t n) {
    uint64_t h = 14695981039346656037ULL;
    for (size_t i = 0; i < n; i++) {
        h ^= (uint64_t)bytes[i];
        h *= 1099511628211ULL;
    }
    return h ? h : 1;
}

static uint64_t lo_map_hash_key(const lo_map* m, const void* key) {
    if (m->is_str_key) {
        const lo_str* s = (const lo_str*)key;
        return lo_hash_bytes((const uint8_t*)lo_cstr(*s), (size_t)s->len);
    }
    return lo_hash_bytes((const uint8_t*)key, m->key_size);
}

static bool lo_map_key_eq(const lo_map* m, const void* a, const void* b) {
    if (m->is_str_key) {
        return lo_str_eq(*(const lo_str*)a, *(const lo_str*)b);
    }
    return memcmp(a, b, m->key_size) == 0;
}

static void lo_map_drop_fn(void* p) {
    lo_map* m = (lo_map*)p;
    for (size_t i = 0; i < m->cap; i++) {
        if (m->entries[i].occupied) {
            free(m->entries[i].key);
            if (m->entries[i].val) free(m->entries[i].val);
        }
    }
    free(m->entries);
}

static inline lo_map* lo_map_new(size_t key_size, size_t val_size, bool is_str_key) {
    lo_map* m = (lo_map*)lo_rc_alloc(sizeof(lo_map), lo_map_drop_fn);
    m->cap = 16;
    m->len = 0;
    m->key_size = key_size > 0 ? key_size : 1;
    m->val_size = val_size;
    m->is_str_key = is_str_key;
    m->entries = (lo_map_entry_t*)calloc(m->cap, sizeof(lo_map_entry_t));
    return m;
}

static inline void lo_map_rehash(lo_map* m) {
    size_t old_cap = m->cap;
    lo_map_entry_t* old = m->entries;
    m->cap *= 2;
    m->entries = (lo_map_entry_t*)calloc(m->cap, sizeof(lo_map_entry_t));
    for (size_t i = 0; i < old_cap; i++) {
        if (old[i].occupied) {
            size_t idx = (size_t)(old[i].hash % m->cap);
            while (m->entries[idx].occupied) idx = (idx + 1) % m->cap;
            m->entries[idx] = old[i];
        }
    }
    free(old);
}

static inline void lo_map_set(lo_map* m, const void* key, const void* val) {
    if ((size_t)(m->len + 1) * 10 >= m->cap * 7) {
        lo_map_rehash(m);
    }
    uint64_t h = lo_map_hash_key(m, key);
    size_t idx = (size_t)(h % m->cap);
    while (m->entries[idx].occupied) {
        if (m->entries[idx].hash == h && lo_map_key_eq(m, m->entries[idx].key, key)) {
            if (m->val_size > 0 && val) {
                memcpy(m->entries[idx].val, val, m->val_size);
            }
            return;
        }
        idx = (idx + 1) % m->cap;
    }
    m->entries[idx].occupied = true;
    m->entries[idx].hash = h;
    m->entries[idx].key = malloc(m->key_size);
    memcpy(m->entries[idx].key, key, m->key_size);
    if (m->val_size > 0) {
        m->entries[idx].val = malloc(m->val_size);
        if (val) memcpy(m->entries[idx].val, val, m->val_size);
    } else {
        m->entries[idx].val = NULL;
    }
    m->len++;
}

static inline void* lo_map_get(const lo_map* m, const void* key) {
    if (!m || m->len == 0) return NULL;
    uint64_t h = lo_map_hash_key(m, key);
    size_t idx = (size_t)(h % m->cap);
    size_t probes = 0;
    while (m->entries[idx].occupied && probes < m->cap) {
        if (m->entries[idx].hash == h && lo_map_key_eq(m, m->entries[idx].key, key)) {
            return m->entries[idx].val ? m->entries[idx].val : (void*)1;
        }
        idx = (idx + 1) % m->cap;
        probes++;
    }
    return NULL;
}

static inline bool lo_map_has(const lo_map* m, const void* key) {
    return lo_map_get(m, key) != NULL;
}

static inline void lo_map_remove(lo_map* m, const void* key) {
    if (!m || m->len == 0) return;
    uint64_t h = lo_map_hash_key(m, key);
    size_t idx = (size_t)(h % m->cap);
    size_t probes = 0;
    while (m->entries[idx].occupied && probes < m->cap) {
        if (m->entries[idx].hash == h && lo_map_key_eq(m, m->entries[idx].key, key)) {
            free(m->entries[idx].key);
            if (m->entries[idx].val) free(m->entries[idx].val);
            m->entries[idx].occupied = false;
            m->entries[idx].key = NULL;
            m->entries[idx].val = NULL;
            m->len--;
            /* Reinsert subsequent cluster entries */
            size_t next = (idx + 1) % m->cap;
            while (m->entries[next].occupied) {
                lo_map_entry_t saved = m->entries[next];
                m->entries[next].occupied = false;
                size_t dest = (size_t)(saved.hash % m->cap);
                while (m->entries[dest].occupied) dest = (dest + 1) % m->cap;
                m->entries[dest] = saved;
                next = (next + 1) % m->cap;
            }
            return;
        }
        idx = (idx + 1) % m->cap;
        probes++;
    }
}

/* =========================================================================
 * 5. Structured Errors (`!{}`, `?!`, `?>`, `!^`, `?`, `?!{}`)
 * ========================================================================= */

typedef struct {
    bool active;
    uint32_t type_id;
    int64_t code;
    lo_str msg;
    void* payload;
    size_t payload_size;
} lo_error_t;

typedef struct lo_try_frame {
    jmp_buf env;
    struct lo_try_frame* prev;
} lo_try_frame_t;

static _Thread_local lo_error_t _lo_err = { false, 0, 0, { NULL, 0, NULL }, NULL, 0 };
static _Thread_local lo_try_frame_t* _lo_try_top = NULL;

static inline void lo_err_clear(void) {
    if (_lo_err.payload) {
        free(_lo_err.payload);
        _lo_err.payload = NULL;
    }
    _lo_err.active = false;
    _lo_err.type_id = 0;
    _lo_err.code = 0;
    _lo_err.msg = lo_str_lit("", 0);
    _lo_err.payload_size = 0;
}

static inline void lo_throw_val(
    uint32_t type_id,
    int64_t code,
    lo_str msg,
    const void* payload,
    size_t payload_size
) {
    lo_err_clear();
    _lo_err.active = true;
    _lo_err.type_id = type_id;
    _lo_err.code = code;
    _lo_err.msg = msg;
    if (payload && payload_size > 0) {
        _lo_err.payload = malloc(payload_size);
        memcpy(_lo_err.payload, payload, payload_size);
        _lo_err.payload_size = payload_size;
    }
}

/* =========================================================================
 * 6. Multi-Core Parallel Pipelines (`~>>`), Async Tasks (`~>{}`), Channels (`[~T]`)
 * ========================================================================= */

typedef struct {
    _Atomic size_t next_idx;
    size_t total;
    size_t in_elem_sz;
    const uint8_t* in_data;
    size_t out_elem_sz;
    uint8_t* out_data;
    void (*fn)(size_t, const void*, void*, void*);
    void* ctx;
} lo_par_job_t;

static void* lo_par_worker_entry(void* arg) {
    lo_par_job_t* job = (lo_par_job_t*)arg;
    for (;;) {
        size_t i = atomic_fetch_add_explicit(&job->next_idx, 1, memory_order_relaxed);
        if (i >= job->total) break;
        const void* inp = job->in_data + i * job->in_elem_sz;
        void* outp = job->out_data ? (job->out_data + i * job->out_elem_sz) : NULL;
        job->fn(i, inp, outp, job->ctx);
    }
    return NULL;
}

static inline void lo_par_for_each(
    size_t total,
    size_t in_elem_sz,
    const void* in_data,
    size_t out_elem_sz,
    void* out_data,
    void (*fn)(size_t, const void*, void*, void*),
    void* ctx
) {
    if (total == 0) return;
    if (total == 1) {
        fn(0, in_data, out_data, ctx);
        return;
    }
    long ncpu = sysconf(_SC_NPROCESSORS_ONLN);
    size_t nthreads = (ncpu > 1) ? (size_t)ncpu : 2;
    if (nthreads > 8) nthreads = 8;
    if (nthreads > total) nthreads = total;

    lo_par_job_t job;
    atomic_init(&job.next_idx, 0);
    job.total = total;
    job.in_elem_sz = in_elem_sz;
    job.in_data = (const uint8_t*)in_data;
    job.out_elem_sz = out_elem_sz;
    job.out_data = (uint8_t*)out_data;
    job.fn = fn;
    job.ctx = ctx;

    pthread_t threads[8];
    for (size_t t = 0; t < nthreads; t++) {
        pthread_create(&threads[t], NULL, lo_par_worker_entry, &job);
    }
    for (size_t t = 0; t < nthreads; t++) {
        pthread_join(threads[t], NULL);
    }
}

typedef struct {
    pthread_t thread;
    bool joined;
    pthread_mutex_t mu;
    void (*fn)(void*, void*);
    void* arg_buf;
    void* ret_buf;
    size_t ret_size;
} lo_task_t;

static void lo_task_drop_fn(void* p) {
    lo_task_t* t = (lo_task_t*)p;
    if (!t->joined) {
        pthread_join(t->thread, NULL);
        t->joined = true;
    }
    pthread_mutex_destroy(&t->mu);
    free(t->arg_buf);
    free(t->ret_buf);
}

static void* lo_task_thread_main(void* p) {
    lo_task_t* t = (lo_task_t*)p;
    t->fn(t->arg_buf, t->ret_buf);
    return NULL;
}

static inline lo_task_t* lo_task_spawn(
    void (*fn)(void*, void*),
    const void* arg,
    size_t arg_size,
    size_t ret_size
) {
    lo_task_t* t = (lo_task_t*)lo_rc_alloc(sizeof(lo_task_t), lo_task_drop_fn);
    pthread_mutex_init(&t->mu, NULL);
    t->joined = false;
    t->fn = fn;
    t->ret_size = ret_size > 0 ? ret_size : 1;
    t->arg_buf = arg_size > 0 ? malloc(arg_size) : NULL;
    if (arg && arg_size > 0) memcpy(t->arg_buf, arg, arg_size);
    t->ret_buf = calloc(1, t->ret_size);
    pthread_create(&t->thread, NULL, lo_task_thread_main, t);
    return t;
}

static inline void lo_task_await(lo_task_t* t, void* out_ret) {
    if (!t) return;
    pthread_mutex_lock(&t->mu);
    if (!t->joined) {
        pthread_join(t->thread, NULL);
        t->joined = true;
    }
    if (out_ret && t->ret_size > 0) {
        memcpy(out_ret, t->ret_buf, t->ret_size);
    }
    pthread_mutex_unlock(&t->mu);
}

typedef struct lo_chan_node {
    struct lo_chan_node* next;
    uint8_t payload[];
} lo_chan_node_t;

typedef struct {
    pthread_mutex_t mu;
    pthread_cond_t cv;
    bool closed;
    size_t elem_size;
    lo_chan_node_t* head;
    lo_chan_node_t* tail;
} lo_chan_t;

static void lo_chan_drop_fn(void* p) {
    lo_chan_t* ch = (lo_chan_t*)p;
    lo_chan_node_t* cur = ch->head;
    while (cur) {
        lo_chan_node_t* nxt = cur->next;
        free(cur);
        cur = nxt;
    }
    pthread_mutex_destroy(&ch->mu);
    pthread_cond_destroy(&ch->cv);
}

static inline lo_chan_t* lo_chan_new(size_t elem_size) {
    lo_chan_t* ch = (lo_chan_t*)lo_rc_alloc(sizeof(lo_chan_t), lo_chan_drop_fn);
    pthread_mutex_init(&ch->mu, NULL);
    pthread_cond_init(&ch->cv, NULL);
    ch->closed = false;
    ch->elem_size = elem_size > 0 ? elem_size : 1;
    ch->head = NULL;
    ch->tail = NULL;
    return ch;
}

static inline void lo_chan_send(lo_chan_t* ch, const void* elem) {
    pthread_mutex_lock(&ch->mu);
    if (ch->closed) {
        pthread_mutex_unlock(&ch->mu);
        lo_panic("send on closed channel", 0);
    }
    lo_chan_node_t* node = (lo_chan_node_t*)malloc(sizeof(lo_chan_node_t) + ch->elem_size);
    node->next = NULL;
    memcpy(node->payload, elem, ch->elem_size);
    if (ch->tail) {
        ch->tail->next = node;
        ch->tail = node;
    } else {
        ch->head = ch->tail = node;
    }
    pthread_cond_signal(&ch->cv);
    pthread_mutex_unlock(&ch->mu);
}

static inline bool lo_chan_recv(lo_chan_t* ch, void* out_elem) {
    pthread_mutex_lock(&ch->mu);
    while (ch->head == NULL && !ch->closed) {
        pthread_cond_wait(&ch->cv, &ch->mu);
    }
    if (ch->head == NULL) {
        pthread_mutex_unlock(&ch->mu);
        return false;
    }
    lo_chan_node_t* node = ch->head;
    ch->head = node->next;
    if (!ch->head) ch->tail = NULL;
    memcpy(out_elem, node->payload, ch->elem_size);
    free(node);
    pthread_mutex_unlock(&ch->mu);
    return true;
}

static inline void lo_chan_close(lo_chan_t* ch) {
    pthread_mutex_lock(&ch->mu);
    ch->closed = true;
    pthread_cond_broadcast(&ch->cv);
    pthread_mutex_unlock(&ch->mu);
}

/* =========================================================================
 * 7. Standard Library C ABI Intrinsics (`io`, `fs`, `time`, `math`)
 * ========================================================================= */

static inline lo_unit_t lo_rt_print_str(const char* s) {
    if (s) fputs(s, stdout);
    fflush(stdout);
    return lo_unit_val();
}

static inline lo_unit_t lo_rt_println_str(const char* s) {
    if (s) puts(s);
    else puts("");
    fflush(stdout);
    return lo_unit_val();
}

static inline lo_unit_t lo_rt_eprintln_str(const char* s) {
    if (s) fprintf(stderr, "%s\n", s);
    return lo_unit_val();
}

static inline lo_str lo_rt_read_line(void) {
    char* line = NULL;
    size_t cap = 0;
    ssize_t n = getline(&line, &cap, stdin);
    if (n < 0) {
        free(line);
        return lo_str_lit("", 0);
    }
    while (n > 0 && (line[n - 1] == '\n' || line[n - 1] == '\r')) {
        n--;
    }
    lo_str res = lo_str_owned_copy(line, (int64_t)n);
    free(line);
    return res;
}

static inline lo_str lo_rt_fs_read_file(const char* path) {
    FILE* f = fopen(path, "rb");
    if (!f) {
        int code = errno ? errno : 2;
        lo_throw_val(0, code, lo_str_from_cstr(strerror(code)), NULL, 0);
        return lo_str_lit("", 0);
    }
    fseek(f, 0, SEEK_END);
    long sz = ftell(f);
    if (sz < 0) sz = 0;
    fseek(f, 0, SEEK_SET);
    char* buf = (char*)lo_rc_alloc((size_t)sz + 1, NULL);
    size_t rd = sz > 0 ? fread(buf, 1, (size_t)sz, f) : 0;
    buf[rd] = '\0';
    fclose(f);
    lo_str s = { buf, (int64_t)rd, buf };
    return s;
}

static inline bool lo_rt_fs_write_file(const char* path, const char* content) {
    FILE* f = fopen(path, "wb");
    if (!f) {
        int code = errno ? errno : 13;
        lo_throw_val(0, code, lo_str_from_cstr(strerror(code)), NULL, 0);
        return false;
    }
    size_t len = content ? strlen(content) : 0;
    if (len > 0 && fwrite(content, 1, len, f) != len) {
        int code = errno ? errno : 5;
        fclose(f);
        lo_throw_val(0, code, lo_str_from_cstr(strerror(code)), NULL, 0);
        return false;
    }
    fclose(f);
    return true;
}

static inline bool lo_rt_fs_exists(const char* path) {
    struct stat st;
    return stat(path, &st) == 0;
}

static inline bool lo_rt_fs_remove(const char* path) {
    if (unlink(path) != 0) {
        int code = errno ? errno : 2;
        lo_throw_val(0, code, lo_str_from_cstr(strerror(code)), NULL, 0);
        return false;
    }
    return true;
}

static inline int64_t lo_rt_time_now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000LL + (int64_t)(ts.tv_nsec / 1000000LL);
}

static inline int64_t lo_rt_time_now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000000LL + (int64_t)(ts.tv_nsec / 1000LL);
}

static inline lo_unit_t lo_rt_time_sleep_ms(int64_t ms) {
    if (ms <= 0) return lo_unit_val();
    struct timespec req;
    req.tv_sec = (time_t)(ms / 1000);
    req.tv_nsec = (long)((ms % 1000) * 1000000L);
    nanosleep(&req, NULL);
    return lo_unit_val();
}

static _Atomic uint64_t _lo_rng_state = 0x853c49e6748fea9bULL;

static inline int64_t lo_rt_math_rand_i64(int64_t low, int64_t high) {
    uint64_t x = atomic_load(&_lo_rng_state);
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    atomic_store(&_lo_rng_state, x);
    uint64_t r = x * 0x2545F4914F6CDD1DULL;
    if (high <= low) return low;
    uint64_t span = (uint64_t)(high - low);
    return low + (int64_t)(r % span);
}

static inline double lo_rt_math_rand_f64(void) {
    uint64_t x = (uint64_t)lo_rt_math_rand_i64(0, 1000000000LL);
    return (double)x / 1000000000.0;
}

#endif /* LO_RUNTIME_H */
