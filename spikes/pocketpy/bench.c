#define _POSIX_C_SOURCE 200809L

#include "pocketpy.h"

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#ifdef __APPLE__
#include <mach/mach_time.h>
#endif

enum { EVAL_SAMPLES = 400, HOST_SAMPLES = 10000 };

static uint64_t now_ns(void) {
#ifdef __APPLE__
    static mach_timebase_info_data_t timebase;
    if(timebase.denom == 0) mach_timebase_info(&timebase);
    return (uint64_t)((double)mach_absolute_time() * (double)timebase.numer /
                      (double)timebase.denom);
#else
    struct timespec value;
    clock_gettime(CLOCK_MONOTONIC, &value);
    return (uint64_t)value.tv_sec * 1000000000ULL + (uint64_t)value.tv_nsec;
#endif
}

static int compare_double(const void *left, const void *right) {
    const double a = *(const double *)left;
    const double b = *(const double *)right;
    return (a > b) - (a < b);
}

static void print_measurement(const char *case_name, double *values, size_t count) {
    double total = 0.0;
    for(size_t i = 0; i < count; i++) total += values[i];
    qsort(values, count, sizeof(double), compare_double);
    const size_t p95_index = ((count * 95) / 100 < count) ? (count * 95) / 100 : count - 1;
    printf("{\"runtime\":\"pocketpy\",\"case\":\"%s\","
           "\"median_microseconds\":%.6f,\"p95_microseconds\":%.6f,"
           "\"total_milliseconds\":%.6f}",
           case_name,
           values[count / 2],
           values[p95_index],
           total / 1000.0);
}

static bool host_add(int argc, py_StackRef argv) {
    if(argc != 1) return false;
    py_newint(py_retval(), py_toint(py_arg(0)) + 1);
    return true;
}

static int cold_start(void) {
    const uint64_t start = now_ns();
    py_initialize();
    const uint64_t elapsed = now_ns() - start;
    printf("%.6f\n", (double)elapsed / 1000.0);
    py_finalize();
    return 0;
}

static int suite(void) {
    double eval_values[EVAL_SAMPLES];
    double host_values[HOST_SAMPLES];
    py_initialize();

    for(size_t i = 0; i < EVAL_SAMPLES; i++) {
        const uint64_t start = now_ns();
        const bool ok = py_exec("20 + 22", "<bench>", EVAL_MODE, NULL);
        eval_values[i] = (double)(now_ns() - start) / 1000.0;
        if(!ok || py_toint(py_retval()) != 42) goto error;
    }

    py_Ref temporary = py_r0();
    py_newnativefunc(temporary, host_add);
    py_setglobal(py_name("host_add"), temporary);
    if(!py_exec("def bench(value): return host_add(value)", "<bench>", EXEC_MODE, NULL)) {
        goto error;
    }
    py_GlobalRef bench = py_getglobal(py_name("bench"));
    py_newint(temporary, 41);
    for(size_t i = 0; i < HOST_SAMPLES; i++) {
        const uint64_t start = now_ns();
        const bool ok = py_call(bench, 1, temporary);
        host_values[i] = (double)(now_ns() - start) / 1000.0;
        if(!ok || py_toint(py_retval()) != 42) goto error;
    }

    printf("[");
    print_measurement("expression_eval", eval_values, EVAL_SAMPLES);
    printf(",");
    print_measurement("warm_host_call", host_values, HOST_SAMPLES);
    printf("]\n");
    py_finalize();
    return 0;

error:
    py_printexc();
    py_finalize();
    return 1;
}

int main(int argc, char **argv) {
    if(argc != 2) {
        fprintf(stderr, "usage: pocketpy-bench --cold|--suite\n");
        return 2;
    }
    if(strcmp(argv[1], "--cold") == 0) return cold_start();
    if(strcmp(argv[1], "--suite") == 0) return suite();
    fprintf(stderr, "unknown mode: %s\n", argv[1]);
    return 2;
}
