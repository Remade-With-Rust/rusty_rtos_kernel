/*
 * The API differential, the C side (docs/plans/api-differential.md, P1).
 *
 * FreeRTOS-Kernel V11.3.1 (the pinned oracle) on a fake port, built ONE-core
 * and TWO-core from this one file (run.sh). A seeded xorshift generates a
 * script of kernel calls, each made "from" a chosen core, and the driver
 * writes one line per step:
 *
 *     <step> c<core> <op> <args...> | r=<result> y=<yield mask> cur=<..> T=<..> s=<..>
 *
 * Left of the bar is the STEP, fully specified: `tests/api_differential.rs`
 * parses it and makes the same call on Kairos. It never sees this RNG, so
 * the grammar lives here alone. Right of the bar is what the C kernel then
 * looked like -- the call's result, the yields it asked for (taken before
 * the line is printed), each core's current task, every app task's state and
 * priority (`eTaskGetState`, `uxTaskPriorityGet`) and the semaphore's count
 * -- and Kairos must print the same. A wrong decision shows on the step that
 * makes it, not three steps later when the schedule finally moves.
 *
 * A call that BLOCKS runs in a ucontext coroutine: the blocking trace hook
 * marks it, and the yield that follows swaps back to the driver, leaving the
 * C call suspended exactly where a real port leaves it. When that task is
 * next current on a core the step is `cont`, and the kernel's own loop
 * carries on with its own timeout state. Kairos does the same through its
 * retry protocol (`Wait::Blocked`: call again when the task next runs).
 *
 * Usage: driver <seed> <steps>
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <ucontext.h>

#include "FreeRTOS.h"
#include "semphr.h"
#include "task.h"
#include "timers.h"

#define SLOTS    10

static uint32_t rng;
static uint32_t next( void )
{
    rng ^= rng << 13;
    rng ^= rng >> 17;
    rng ^= rng << 5;
    return rng;
}

static TaskHandle_t app[ SLOTS ];
static unsigned created;
static SemaphoreHandle_t sem;

static void body( void * p )
{
    ( void ) p;

    for( ; ; )
    {
    }
}

/* ------------------------------------------------ blocking calls -- */

extern volatile int fake_blocking;
extern void ( * fake_yield_hook )( void );

enum call_kind { CALL_TAKE };

struct call
{
    enum call_kind kind;
    TickType_t ticks;
};

#define CORO_STACK    ( 256 * 1024 )
static ucontext_t driver_ctx;
static ucontext_t coro[ SLOTS ];
static char * coro_stack[ SLOTS ];
static struct call coro_call[ SLOTS ];
static int pending[ SLOTS ];
static int coro_done[ SLOTS ];
static long coro_result[ SLOTS ];
static int in_coro = -1;
static int coro_start_slot;

/* A yield of the core the coroutine runs on: if the call has just blocked,
 * leave the kernel here. Any other yield (a preemption) is only recorded. */
static void on_yield( void )
{
    if( ( in_coro >= 0 ) && fake_blocking )
    {
        fake_blocking = 0;
        swapcontext( &coro[ in_coro ], &driver_ctx );
    }
}

static void coro_main( void )
{
    int s = coro_start_slot;
    struct call * c = &coro_call[ s ];

    switch( c->kind )
    {
        case CALL_TAKE:
            coro_result[ s ] = xSemaphoreTake( sem, c->ticks );
            break;
    }

    coro_done[ s ] = 1;
    /* returning resumes uc_link: the driver */
}

/* Run slot s's coroutine until it completes or blocks again: the call's
 * result, or 2 for "blocked". */
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

static long start_call( int s,
                        struct call c )
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
    coro_call[ s ] = c;
    coro_start_slot = s;
    makecontext( &coro[ s ], coro_main, 0 );
    return run_coro( s );
}

/* ------------------------------------------------ the observation -- */

static int slot_of( TaskHandle_t h )
{
    for( int i = 0; i < SLOTS; i++ )
    {
        if( ( app[ i ] != NULL ) && ( app[ i ] == h ) )
        {
            return i;
        }
    }

    return -1;
}

static const char * name_of( TaskHandle_t h )
{
    return h ? pcTaskGetName( h ) : "-";
}

static void switch_core( int c )
{
    fake_core = c;
    #if configNUMBER_OF_CORES > 1
        vTaskSwitchContext( c );
    #else
        vTaskSwitchContext();
    #endif
}

static char state_char( eTaskState s )
{
    switch( s )
    {
        case eRunning:   return 'X';
        case eReady:     return 'R';
        case eBlocked:   return 'B';
        case eSuspended: return 'S';
        case eDeleted:   return 'D';
        default:         return '?';
    }
}

/* Take the step's yields (lowest core first), then print the line. The
 * observation itself is made from core 0. */
static void line( unsigned step,
                  int core,
                  const char * op,
                  long r,
                  unsigned mask )
{
    for( int c = 0; c < configNUMBER_OF_CORES; c++ )
    {
        if( mask & ( 1u << c ) )
        {
            switch_core( c );
        }
    }

    fake_core = 0;
    printf( "%u c%d %s | r=%ld y=%u cur=", step, core, op, r, mask );

    for( int c = 0; c < configNUMBER_OF_CORES; c++ )
    {
        printf( "%s%s", c ? "," : "", name_of( xTaskGetCurrentTaskHandleForCore( c ) ) );
    }

    printf( " T=" );

    for( int i = 0; i < SLOTS; i++ )
    {
        if( app[ i ] == NULL )
        {
            printf( "-" );
        }
        else
        {
            printf( "%c%lu", state_char( eTaskGetState( app[ i ] ) ),
                    ( unsigned long ) uxTaskPriorityGet( app[ i ] ) );
        }

        printf( i + 1 < SLOTS ? "." : "" );
    }

    printf( " s=%lu\n", ( unsigned long ) uxSemaphoreGetCount( sem ) );
}

/* ------------------------------------------------------- the script -- */

int main( int argc,
          char ** argv )
{
    char op[ 64 ];
    unsigned steps = ( argc > 2 ) ? ( unsigned ) strtoul( argv[ 2 ], NULL, 0 ) : 20000u;

    rng = ( argc > 1 ) ? ( uint32_t ) strtoul( argv[ 1 ], NULL, 0 ) : 0x2545f491u;
    fake_yield_hook = on_yield;

    sem = xSemaphoreCreateBinary();

    UBaseType_t init[ 4 ];

    for( int i = 0; i < 4; i++ )
    {
        char n[ 8 ];
        snprintf( n, sizeof n, "t%u", created++ );
        init[ i ] = ( UBaseType_t ) ( 1 + next() % 3 );
        xTaskCreate( body, n, configMINIMAL_STACK_SIZE, NULL, init[ i ], &app[ i ] );
    }

    vTaskStartScheduler();
    fake_core = 0;
    vTaskSuspend( xTimerGetTimerDaemonTaskHandle() );
    fake_yields = 0;

    for( int c = 0; c < configNUMBER_OF_CORES; c++ )
    {
        switch_core( c );
    }

    /* The setup above is the same on both sides and is not a step; the
     * first line records where it left the kernel. */
    printf( "seed=0x%08lx cores=%d steps=%u init=%lu,%lu,%lu,%lu\n",
            ( unsigned long ) ( argc > 1 ? strtoul( argv[ 1 ], NULL, 0 ) : 0x2545f491u ),
            configNUMBER_OF_CORES, steps, ( unsigned long ) init[ 0 ], ( unsigned long ) init[ 1 ],
            ( unsigned long ) init[ 2 ], ( unsigned long ) init[ 3 ] );
    line( 0, 0, "start", 0, 0 );

    for( unsigned step = 1; step <= steps; step++ )
    {
        int core = ( int ) ( next() % configNUMBER_OF_CORES );
        unsigned kind = next() % 100;
        unsigned slot = next() % SLOTS;
        unsigned arg = next();
        long r = 0;
        TaskHandle_t t = app[ slot ];

        fake_core = core;
        fake_yields = 0;

        /* A task with a call suspended inside the kernel runs nothing else
         * until that call returns: if it is current here, the step is its
         * continuation. */
        int cur_slot = slot_of( xTaskGetCurrentTaskHandleForCore( core ) );

        if( ( cur_slot >= 0 ) && pending[ cur_slot ] )
        {
            r = run_coro( cur_slot );
            line( step, core, "cont", r, fake_yields );
            continue;
        }

        if( kind < 10 )
        {
            if( t == NULL )
            {
                char n[ 8 ];
                snprintf( n, sizeof n, "t%u", created++ );
                UBaseType_t p = arg % 4;
                r = xTaskCreate( body, n, configMINIMAL_STACK_SIZE, NULL, p, &app[ slot ] );
                snprintf( op, sizeof op, "create %u %s %lu", slot, n, ( unsigned long ) p );
            }
            else
            {
                snprintf( op, sizeof op, "noop" );
            }
        }
        else if( kind < 18 )
        {
            if( t != NULL )
            {
                snprintf( op, sizeof op, "delete %u", slot );
                app[ slot ] = NULL;
                pending[ slot ] = 0; /* its suspended call is abandoned */
                vTaskDelete( t );
            }
            else
            {
                snprintf( op, sizeof op, "noop" );
            }
        }
        else if( kind < 30 )
        {
            if( t != NULL )
            {
                vTaskSuspend( t );
                snprintf( op, sizeof op, "suspend %u", slot );
            }
            else
            {
                snprintf( op, sizeof op, "noop" );
            }
        }
        else if( kind < 42 )
        {
            if( t != NULL )
            {
                vTaskResume( t );
                snprintf( op, sizeof op, "resume %u", slot );
            }
            else
            {
                snprintf( op, sizeof op, "noop" );
            }
        }
        else if( kind < 54 )
        {
            if( t != NULL )
            {
                UBaseType_t p = arg % 4;
                vTaskPrioritySet( t, p );
                snprintf( op, sizeof op, "prio %u %lu", slot, ( unsigned long ) p );
            }
            else
            {
                snprintf( op, sizeof op, "noop" );
            }
        }
        else if( kind < 64 )
        {
            if( cur_slot >= 0 )
            {
                TickType_t d = 1 + arg % 5;
                vTaskDelay( d );
                snprintf( op, sizeof op, "delay %lu", ( unsigned long ) d );
            }
            else
            {
                snprintf( op, sizeof op, "noop" );
            }
        }
        else if( kind < 72 )
        {
            r = xSemaphoreGive( sem );
            snprintf( op, sizeof op, "give" );
        }
        else if( kind < 78 )
        {
            BaseType_t woken = pdFALSE;
            fake_in_isr = 1;
            r = xSemaphoreGiveFromISR( sem, &woken );
            fake_in_isr = 0;
            portYIELD_FROM_ISR( woken );
            snprintf( op, sizeof op, "give_isr" );
        }
        else if( kind < 86 )
        {
            if( cur_slot >= 0 )
            {
                TickType_t ticks = ( arg % 3 == 0 ) ? 0 : 1 + ( arg >> 2 ) % 6;

                if( ticks > 0 )
                {
                    struct call c = { CALL_TAKE, ticks };
                    r = start_call( cur_slot, c );
                }
                else
                {
                    r = xSemaphoreTake( sem, 0 );
                }

                snprintf( op, sizeof op, "take %lu", ( unsigned long ) ticks );
            }
            else
            {
                snprintf( op, sizeof op, "noop" );
            }
        }
        else if( kind < 96 )
        {
            /* As every SMP port's tick handler does (RP2040's included):
             * xTaskIncrementTick inside the ISR critical section, on core 0. */
            core = 0;
            fake_core = 0;
            fake_in_isr = 1;
            UBaseType_t saved = taskENTER_CRITICAL_FROM_ISR();
            r = xTaskIncrementTick();
            taskEXIT_CRITICAL_FROM_ISR( saved );
            fake_in_isr = 0;

            if( r )
            {
                portYIELD();
            }

            snprintf( op, sizeof op, "tick" );
        }
        else
        {
            portYIELD();
            snprintf( op, sizeof op, "yield" );
        }

        line( step, core, op, r, fake_yields );
    }

    return 0;
}
