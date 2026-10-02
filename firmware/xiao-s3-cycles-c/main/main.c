/*
 * The C arm of `xiao-s3-cycles`: the same four rows, the same instrument,
 * the same part -- with FreeRTOS as ESP-IDF ships it instead of Kairos.
 *
 *   tick                  xTaskIncrementTick()
 *   tick (one delayed)    the same, with a task on the delayed list
 *   switch                vTaskSwitchContext() -- choose and commit, no
 *                         register swap (the Kairos row is the same)
 *   ISR-API wake          xQueueSendFromISR(); vTaskSwitchContext();
 *                         xQueueReceive(q, 0) -- inline, NO interrupt taken
 *
 * Kairos's cell calls its kernel directly on a stackless port. The faithful
 * C equivalent is to call the same kernel entry points directly, with
 * interrupts masked so the real tick and the real scheduler cannot run in
 * between: from inside the bracket, the kernel's own work is the only thing
 * timed. The task set is mirrored: the measuring task and one more ready task
 * share priority 2, so time slicing sees a list of two, as Kairos's does.
 *
 * Clock: CCOUNT, one cycle of resolution at 240 MHz. Median of 512, the
 * bracket tax (an empty CCOUNT pair) measured and subtracted.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "freertos/FreeRTOS.h"
#include "freertos/queue.h"
#include "freertos/task.h"
#include "esp_cpu.h"
#include "esp_rom_sys.h"

#define SAMPLES 512

/* The upstream (Amazon SMP) kernel takes a core id even with one core
 * configured; ESP-IDF's own fork does not. */
#if defined(configNUMBER_OF_CORES) && (configNUMBER_OF_CORES > 1)
#define SWITCH() vTaskSwitchContext(0)
#else
#define SWITCH() vTaskSwitchContext()
#endif

static uint32_t samples[SAMPLES];

typedef struct {
    uint32_t median, min, max;
    int below;
} row_t;

static int cmp_u32(const void *a, const void *b)
{
    uint32_t x = *(const uint32_t *)a, y = *(const uint32_t *)b;
    return (x > y) - (x < y);
}

static row_t summarise(uint32_t tax)
{
    qsort(samples, SAMPLES, sizeof(uint32_t), cmp_u32);
    row_t r;
    uint32_t raw = samples[SAMPLES / 2];
    r.below = raw <= tax;
    r.median = raw > tax ? raw - tax : 0;
    r.min = samples[0] > tax ? samples[0] - tax : 0;
    r.max = samples[SAMPLES - 1] > tax ? samples[SAMPLES - 1] - tax : 0;
    return r;
}

static void report(const char *name, row_t r)
{
    if (r.below) {
        printf("  %-26s BELOW RESOLUTION\n", name);
    } else {
        printf("  %-26s median=%-6lu min=%-6lu max=%lu\n", name, (unsigned long)r.median,
               (unsigned long)r.min, (unsigned long)r.max);
    }
}

static void parked(void *arg)
{
    (void)arg;
    for (;;) {
        vTaskDelay(portMAX_DELAY);
    }
}

static void delayer(void *arg)
{
    (void)arg;
    /* Onto the DELAYED list (a finite wait), not the suspended one. */
    for (;;) {
        vTaskDelay(1000000);
    }
}

static TaskHandle_t self;

/* After `vTaskSwitchContext` the kernel believes another task is running
 * while this code continues -- the same position Kairos's stackless
 * measurement is in. Put it back before anything blocks or yields. */
static void restore_current(void)
{
    for (int i = 0; i < 8 && xTaskGetCurrentTaskHandle() != self; i++) {
        SWITCH();
    }
}

static void measure(void *arg)
{
    (void)arg;
    self = xTaskGetCurrentTaskHandle();
    QueueHandle_t q = xQueueCreate(1, sizeof(uint32_t));
    /* The second ready task at priority 2, so a switch has a decision. */
    TaskHandle_t other;
    xTaskCreate(parked, "other", 2048, NULL, 2, &other);
    /* `other` never runs: the tick and every yield are masked below. */
    vTaskDelay(10);

    uint32_t a, b;

    /* The bracket tax. */
    for (int i = 0; i < SAMPLES; i++) {
        a = esp_cpu_get_cycle_count();
        b = esp_cpu_get_cycle_count();
        samples[i] = b - a;
    }
    qsort(samples, SAMPLES, sizeof(uint32_t), cmp_u32);
    uint32_t tax = samples[SAMPLES / 2];

    portDISABLE_INTERRUPTS();

    for (int i = 0; i < SAMPLES; i++) {
        a = esp_cpu_get_cycle_count();
        (void)xTaskIncrementTick();
        b = esp_cpu_get_cycle_count();
        samples[i] = b - a;
    }
    row_t tick = summarise(tax);

    for (int i = 0; i < SAMPLES; i++) {
        a = esp_cpu_get_cycle_count();
        SWITCH();
        b = esp_cpu_get_cycle_count();
        samples[i] = b - a;
    }
    row_t sw = summarise(tax);
    restore_current();

    int wake_failures = 0;
    for (int i = 0; i < SAMPLES; i++) {
        uint32_t v = 0x5a5a, got = 0;
        BaseType_t woken = pdFALSE;
        a = esp_cpu_get_cycle_count();
        (void)xQueueSendFromISR(q, &v, &woken);
        SWITCH();
        BaseType_t ok = xQueueReceive(q, &got, 0);
        b = esp_cpu_get_cycle_count();
        if (ok == pdTRUE && got == 0x5a5a) {
            samples[i] = b - a;
        } else {
            wake_failures++;
            samples[i] = UINT32_MAX;
        }
    }
    row_t wake = summarise(tax);
    restore_current();

    portENABLE_INTERRUPTS();

    /* The delayed row: a task really on the delayed list, then the tick. */
    TaskHandle_t d;
    xTaskCreate(delayer, "delay", 2048, NULL, 3, &d);
    vTaskDelay(10);
    int delayed_ok = eTaskGetState(d) == eBlocked;
    portDISABLE_INTERRUPTS();
    for (int i = 0; i < SAMPLES; i++) {
        a = esp_cpu_get_cycle_count();
        (void)xTaskIncrementTick();
        b = esp_cpu_get_cycle_count();
        samples[i] = b - a;
    }
    row_t tickd = summarise(tax);
    restore_current();
    portENABLE_INTERRUPTS();

    printf("\n=== xiao-s3-cycles-c: FreeRTOS %s (ESP-IDF), the C arm ===\n", tskKERNEL_VERSION_NUMBER);
    printf("clock   CCOUNT at %lu MHz, median of %d, bracket tax %lu cycles subtracted\n",
           (unsigned long)(esp_rom_get_cpu_ticks_per_us()), SAMPLES, (unsigned long)tax);
    printf("cycles per operation, on the part:\n");
    report("tick (nothing delayed)", tick);
    report("tick (one task delayed)", tickd);
    report("switch", sw);
    report("ISR-API wake -> task has it", wake);
    printf("  wake failures %d, delayed task really delayed: %s\n", wake_failures,
           delayed_ok ? "yes" : "NO");
    printf("RESULT: %s -- tick %lu/%lu, switch %lu, ISR-API wake %lu cycles\n",
           (wake_failures == 0 && delayed_ok && !tick.below && !sw.below) ? "PASS" : "FAIL",
           (unsigned long)tick.median, (unsigned long)tickd.median, (unsigned long)sw.median,
           (unsigned long)wake.median);
    for (;;) {
        vTaskDelay(portMAX_DELAY);
    }
}

void app_main(void)
{
    xTaskCreate(measure, "measure", 4096, NULL, 2, NULL);
}
