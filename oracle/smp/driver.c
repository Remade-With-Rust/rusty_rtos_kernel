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
 *
 * Built with -DBLOCKING_WAITS (run.sh's second binary, `smp_block.trace`), a
 * take may BLOCK. A single-threaded driver cannot simply call a blocking
 * `xSemaphoreTake`: the fake `portYIELD` returns straight back into its
 * `for(;;)`, which would place the task on the event list a second time. So
 * each app task gets a ucontext coroutine. `traceBLOCKING_ON_QUEUE_RECEIVE`
 * marks the block, and the yield that follows swaps back to the driver --
 * leaving the C call suspended exactly where a real port leaves it. When that
 * task is next current on some core, the step is `cont`: the driver swaps
 * back in, and the kernel's own loop carries on from the yield, with its own
 * timeout state.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifdef BLOCKING_WAITS
#include <ucontext.h>
#endif

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

#ifdef BLOCKING_WAITS
extern volatile int fake_blocking;
extern void ( * fake_yield_hook )( void );

#define CORO_STACK ( 256 * 1024 )
static ucontext_t driver_ctx;
static ucontext_t coro[ SLOTS ];
static char *coro_stack[ SLOTS ];
static int pending[ SLOTS ];       /* a take is suspended inside the kernel */
static int coro_done[ SLOTS ];
static long coro_result[ SLOTS ];
static TickType_t coro_ticks[ SLOTS ];
static int in_coro = -1;
static int coro_start_slot;

/* A yield of the core the coroutine runs on: if the take has just blocked,
 * leave the kernel here. Any other yield (a preemption) is only recorded. */
static void on_yield( void )
{
    if( in_coro >= 0 && fake_blocking )
    {
        fake_blocking = 0;
        int s = in_coro;
        swapcontext( &coro[ s ], &driver_ctx );
    }
}

static void coro_main( void )
{
    int s = coro_start_slot;
    coro_result[ s ] = xSemaphoreTake( sem, coro_ticks[ s ] );
    coro_done[ s ] = 1;
    /* returning resumes uc_link: the driver */
}

/* Run slot s's coroutine until it completes or blocks again.
 * Answers 1 / 0 (the take's result) or 2 (blocked). */
static long run_coro( int s )
{
    in_coro = s;
    fake_blocking = 0;
    swapcontext( &driver_ctx, &coro[ s ] );
    in_coro = -1;
    if( coro_done[ s ] )
    {
        pending[ s ] = 0;
        return coro_result[ s ];
    }
    pending[ s ] = 1;
    return 2;
}

static long start_take( int s, TickType_t ticks )
{
    if( coro_stack[ s ] == NULL )
    {
        coro_stack[ s ] = malloc( CORO_STACK );
    }
    getcontext( &coro[ s ] );
    coro[ s ].uc_stack.ss_sp = coro_stack[ s ];
    coro[ s ].uc_stack.ss_size = CORO_STACK;
    coro[ s ].uc_link = &driver_ctx;
    coro_done[ s ] = 0;
    coro_ticks[ s ] = ticks;
    coro_start_slot = s;
    makecontext( &coro[ s ], coro_main, 0 );
    return run_coro( s );
}

static int slot_of( TaskHandle_t h )
{
    for( int i = 0; i < SLOTS; i++ )
    {
        if( app[ i ] && app[ i ] == h )
        {
            return i;
        }
    }
    return -1;
}
#endif

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
#ifdef BLOCKING_WAITS
    fake_yield_hook = on_yield;
#endif
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

#ifdef BLOCKING_WAITS
        /* A task with a take suspended inside the kernel runs nothing else
         * until that take returns: if it is current here, the step is its
         * continuation. */
        int cur_slot = slot_of( xTaskGetCurrentTaskHandleForCore( core ) );
        if( cur_slot >= 0 && pending[ cur_slot ] )
        {
            r = run_coro( cur_slot );
            snprintf( op, sizeof op, "cont %s", name_of( app[ cur_slot ] ) );
            line( step, core, op, r, fake_yields );
            continue;
        }
#endif

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
#ifdef BLOCKING_WAITS
                pending[ slot ] = 0;   /* its suspended take is abandoned */
#endif
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
#ifdef BLOCKING_WAITS
                TickType_t ticks = ( arg % 3 == 0 ) ? 0 : 1 + ( arg >> 2 ) % 6;
                if( ticks > 0 ) {
                    r = start_take( slot_of( cur ), ticks );
                    snprintf(op, sizeof op, "takeb %s %lu", name_of(cur), (unsigned long)ticks);
                } else
#endif
                {
                r = xSemaphoreTake(sem, 0);
                snprintf(op, sizeof op, "take %s", name_of(cur));
                }
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
