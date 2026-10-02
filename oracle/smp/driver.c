/*
 * SMP scheduler differential, the C side.
 *
 * FreeRTOS-Kernel V11.3.1 (the pinned oracle) built with two cores on a fake
 * port (see portmacro.h). A 32-bit xorshift generates a script of operations,
 * each made "from" a chosen core; after every step the driver prints both
 * cores' current tasks, the yield mask the kernel asked for, and the call's
 * result, then runs vTaskSwitchContext for every core in the mask.
 *
 * `tests/smp_differential.rs` runs the SAME script against Kairos and must
 * print the same lines. Any change here must be mirrored there.
 */
#include <stdio.h>
#include <string.h>

#include "FreeRTOS.h"
#include "semphr.h"
#include "task.h"
#include "timers.h"

#define STEPS 20000
#define SLOTS 10

static uint32_t rng = 0x2545f491u;
static uint32_t next(void)
{
    rng ^= rng << 13;
    rng ^= rng >> 17;
    rng ^= rng << 5;
    return rng;
}

static TaskHandle_t app[SLOTS];
static unsigned created;
static SemaphoreHandle_t sem;

static void body(void *p)
{
    (void)p;
    for (;;) {
    }
}

static const char *name_of(TaskHandle_t h)
{
    return h ? pcTaskGetName(h) : "-";
}

static int is_app(TaskHandle_t h)
{
    for (int i = 0; i < SLOTS; i++) {
        if (app[i] && app[i] == h) {
            return 1;
        }
    }
    return 0;
}

static void switch_core(int c)
{
    fake_core = c;
    vTaskSwitchContext(c);
}

/* Switch every core the step asked to yield, lowest first. */
static void settle(unsigned mask)
{
    for (int c = 0; c < 2; c++) {
        if (mask & (1u << c)) {
            switch_core(c);
        }
    }
}

static void line(unsigned step, int core, const char *op, long r, unsigned mask)
{
    settle(mask);
    printf("%u core=%d %s r=%ld y=%u c0=%s c1=%s\n", step, core, op, r, mask,
           name_of(xTaskGetCurrentTaskHandleForCore(0)),
           name_of(xTaskGetCurrentTaskHandleForCore(1)));
}

int main(void)
{
    char op[48];
    sem = xSemaphoreCreateBinary();
    for (int i = 0; i < 4; i++) {
        char n[8];
        snprintf(n, sizeof n, "t%u", created++);
        xTaskCreate(body, n, configMINIMAL_STACK_SIZE, NULL, (UBaseType_t)(1 + next() % 3), &app[i]);
    }
    vTaskStartScheduler();
    fake_core = 0;
    vTaskSuspend(xTimerGetTimerDaemonTaskHandle());
    fake_yields = 0;
    switch_core(0);
    switch_core(1);
    line(0, 0, "start", 0, 0);

    for (unsigned step = 1; step <= STEPS; step++) {
        int core = (int)(next() % 2);
        unsigned kind = next() % 100;
        unsigned slot = next() % SLOTS;
        unsigned arg = next();
        long r = 0;
        fake_core = core;
        fake_yields = 0;
        TaskHandle_t t = app[slot];

        if (kind < 10) {
            if (t == NULL) {
                char n[8];
                snprintf(n, sizeof n, "t%u", created++);
                UBaseType_t p = arg % 4;
                r = xTaskCreate(body, n, configMINIMAL_STACK_SIZE, NULL, p, &app[slot]);
                snprintf(op, sizeof op, "create %u %s p%lu", slot, n, (unsigned long)p);
            } else {
                snprintf(op, sizeof op, "noop");
            }
        } else if (kind < 18) {
            if (t) {
                snprintf(op, sizeof op, "delete %u %s", slot, name_of(t));
                app[slot] = NULL;
                vTaskDelete(t);
            } else {
                snprintf(op, sizeof op, "noop");
            }
        } else if (kind < 30) {
            if (t) {
                vTaskSuspend(t);
                snprintf(op, sizeof op, "suspend %s", name_of(t));
            } else {
                snprintf(op, sizeof op, "noop");
            }
        } else if (kind < 42) {
            if (t) {
                vTaskResume(t);
                snprintf(op, sizeof op, "resume %s", name_of(t));
            } else {
                snprintf(op, sizeof op, "noop");
            }
        } else if (kind < 54) {
            if (t) {
                UBaseType_t p = arg % 4;
                vTaskPrioritySet(t, p);
                snprintf(op, sizeof op, "prio %s %lu", name_of(t), (unsigned long)p);
            } else {
                snprintf(op, sizeof op, "noop");
            }
        } else if (kind < 64) {
            TaskHandle_t cur = xTaskGetCurrentTaskHandleForCore(core);
            if (is_app(cur)) {
                TickType_t d = 1 + arg % 5;
                vTaskDelay(d);
                snprintf(op, sizeof op, "delay %s %lu", name_of(cur), (unsigned long)d);
            } else {
                snprintf(op, sizeof op, "noop");
            }
        } else if (kind < 72) {
            r = xSemaphoreGive(sem);
            snprintf(op, sizeof op, "give");
        } else if (kind < 78) {
            BaseType_t woken = pdFALSE;
            fake_in_isr = 1;
            r = xSemaphoreGiveFromISR(sem, &woken);
            fake_in_isr = 0;
            portYIELD_FROM_ISR(woken);
            snprintf(op, sizeof op, "give_isr");
        } else if (kind < 86) {
            TaskHandle_t cur = xTaskGetCurrentTaskHandleForCore(core);
            if (is_app(cur)) {
                r = xSemaphoreTake(sem, 0);
                snprintf(op, sizeof op, "take %s", name_of(cur));
            } else {
                snprintf(op, sizeof op, "noop");
            }
        } else if (kind < 96) {
            core = 0;
            fake_core = 0;
            /* As every SMP port's tick handler does (RP2040's included):
             * xTaskIncrementTick inside the ISR critical section. */
            fake_in_isr = 1;
            UBaseType_t saved = taskENTER_CRITICAL_FROM_ISR();
            r = xTaskIncrementTick();
            taskEXIT_CRITICAL_FROM_ISR(saved);
            fake_in_isr = 0;
            if (r) {
                portYIELD();
            }
            snprintf(op, sizeof op, "tick");
        } else {
            portYIELD();
            snprintf(op, sizeof op, "yield");
        }
        line(step, core, op, r, fake_yields);
    }
    return 0;
}
