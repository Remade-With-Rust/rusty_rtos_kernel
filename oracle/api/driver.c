/*
 * The API differential, the C side (docs/plans/api-differential.md, P1).
 *
 * FreeRTOS-Kernel V11.3.1 (the pinned oracle) on a fake port, built ONE-core
 * and TWO-core from this one file (run.sh). A seeded xorshift generates a
 * script of kernel calls, each made "from" a chosen core, and the driver
 * writes one line per step:
 *
 *     <step> c<core> <op> <args...> | r=<result> y=<yields> cur=<..> T=<..> s=<..> Q=<..>
 *
 * Left of the bar is the STEP, fully specified: `tests/api_differential.rs`
 * parses it and makes the same call on Kairos. It never sees this RNG, so
 * the grammar lives here alone. Right of the bar is what the C kernel then
 * looked like -- the call's result, the yields it asked for (taken before
 * the line is printed), each core's current task, every app task's state and
 * priority (`eTaskGetState`, `uxTaskPriorityGet`), the semaphore's count and
 * every queue's message count -- and Kairos must print the same. A wrong
 * decision shows on the step that makes it.
 *
 * Results: 1 / 0 for pass / fail (pdPASS, errQUEUE_FULL, pdFALSE), -1 for a
 * create that failed, 2 for "blocked, now pending", and a RECEIVED VALUE
 * itself for a receive or peek that got one -- values are 10..99, so they
 * never collide with the codes, and an ordering bug shows on the receive.
 *
 * A call that BLOCKS runs in a ucontext coroutine: the blocking trace hook
 * marks it, and the yield that follows swaps back to the driver, leaving the
 * C call suspended exactly where a real port leaves it. When that task is
 * next current on a core the step is `cont`, and the kernel's own loop
 * carries on with its own timeout state. Kairos does the same through its
 * retry protocol (`Wait::Blocked`: call again when the task next runs).
 *
 * The script never asks the C for undefined behaviour (plan decision D1):
 * no call on a deleted object, no overwrite of a queue longer than one, no
 * deleting a queue a task is blocked on.
 *
 * Usage: driver <seed> <steps>
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <ucontext.h>

#include "FreeRTOS.h"
#include "queue.h"
#include "semphr.h"
#include "task.h"
#include "timers.h"

#define SLOTS     10 /* app tasks */
#define QSLOTS    3  /* queues */

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
static QueueHandle_t queue[ QSLOTS ];
static SemaphoreHandle_t mutex;  /* a mutex: priority inheritance */
static SemaphoreHandle_t rmutex; /* a recursive mutex */
static SemaphoreHandle_t csem;   /* a counting semaphore, max 3, from 1 */
static unsigned queue_len[ QSLOTS ];

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

enum call_kind { CALL_TAKE, CALL_QSEND, CALL_QSENDF, CALL_QRECV, CALL_QPEEK, CALL_MTAKE, CALL_RTAKE, CALL_CTAKE };

static int is_queue_call( enum call_kind k )
{
    return ( k == CALL_QSEND ) || ( k == CALL_QSENDF ) || ( k == CALL_QRECV ) || ( k == CALL_QPEEK );
}

struct call
{
    enum call_kind kind;
    TickType_t ticks;
    int q;          /* queue slot, for the queue calls */
    uint32_t value; /* what a send sends */
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

/* A receive or peek's result: the value, or 0 for "nothing". */
static long got( BaseType_t ok,
                 const uint32_t * v )
{
    return ok ? ( long ) *v : 0;
}

static void coro_main( void )
{
    int s = coro_start_slot;
    struct call * c = &coro_call[ s ];
    uint32_t v = 0;

    switch( c->kind )
    {
        case CALL_TAKE:
            coro_result[ s ] = xSemaphoreTake( sem, c->ticks );
            break;

        case CALL_QSEND:
            coro_result[ s ] = xQueueSendToBack( queue[ c->q ], &c->value, c->ticks );
            break;

        case CALL_QSENDF:
            coro_result[ s ] = xQueueSendToFront( queue[ c->q ], &c->value, c->ticks );
            break;

        case CALL_QRECV:
            coro_result[ s ] = got( xQueueReceive( queue[ c->q ], &v, c->ticks ), &v );
            break;

        case CALL_QPEEK:
            coro_result[ s ] = got( xQueuePeek( queue[ c->q ], &v, c->ticks ), &v );
            break;

        case CALL_MTAKE:
            coro_result[ s ] = xSemaphoreTake( mutex, c->ticks );
            break;

        case CALL_RTAKE:
            coro_result[ s ] = xSemaphoreTakeRecursive( rmutex, c->ticks );
            break;

        case CALL_CTAKE:
            coro_result[ s ] = xSemaphoreTake( csem, c->ticks );
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

/* Whether any task is blocked inside a call on queue slot q: such a queue
 * must not be deleted (the C would leave its waiters on a freed list). */
static int queue_has_waiter( int q )
{
    for( int i = 0; i < SLOTS; i++ )
    {
        if( pending[ i ] && is_queue_call( coro_call[ i ].kind ) && ( coro_call[ i ].q == q ) )
        {
            return 1;
        }
    }

    return 0;
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

    printf( " s=%lu Q=", ( unsigned long ) uxSemaphoreGetCount( sem ) );

    for( int q = 0; q < QSLOTS; q++ )
    {
        if( queue[ q ] == NULL )
        {
            printf( "-" );
        }
        else
        {
            printf( "%lu", ( unsigned long ) uxQueueMessagesWaiting( queue[ q ] ) );
        }

        printf( q + 1 < QSLOTS ? "." : "" );
    }

    printf( " M=%s R=%s C=%lu\n", name_of( xSemaphoreGetMutexHolder( mutex ) ),
            name_of( xSemaphoreGetMutexHolder( rmutex ) ),
            ( unsigned long ) uxSemaphoreGetCount( csem ) );
}

/* Whether task t holds either mutex: such a task is not deleted (the C would
 * leave the mutex naming a freed TCB). */
static int holds_a_mutex( TaskHandle_t t )
{
    return ( xSemaphoreGetMutexHolder( mutex ) == t ) || ( xSemaphoreGetMutexHolder( rmutex ) == t );
}

/* -------------------------------------------------------- the families -- */

/* A blocking call's ticks: a third of the time none (the call returns at
 * once), otherwise 1..6. */
static TickType_t block_ticks( unsigned arg )
{
    return ( arg % 3 == 0 ) ? 0 : 1 + ( arg >> 2 ) % 6;
}

/* Make a call from task slot `s` that may block: through a coroutine if it
 * has ticks, directly if not. */
static long call_from_task( int s,
                            struct call c )
{
    if( c.ticks > 0 )
    {
        return start_call( s, c );
    }

    coro_call[ s ] = c; /* not pending: only the arguments, for queue_has_waiter */
    uint32_t v = 0;

    switch( c.kind )
    {
        case CALL_TAKE:   return xSemaphoreTake( sem, 0 );
        case CALL_QSEND:  return xQueueSendToBack( queue[ c.q ], &c.value, 0 );
        case CALL_QSENDF: return xQueueSendToFront( queue[ c.q ], &c.value, 0 );
        case CALL_QRECV:  return got( xQueueReceive( queue[ c.q ], &v, 0 ), &v );
        case CALL_QPEEK:  return got( xQueuePeek( queue[ c.q ], &v, 0 ), &v );
        case CALL_MTAKE:  return xSemaphoreTake( mutex, 0 );
        case CALL_RTAKE:  return xSemaphoreTakeRecursive( rmutex, 0 );
        case CALL_CTAKE:  return xSemaphoreTake( csem, 0 );
    }

    return 0;
}

static void isr_enter( void )
{
    fake_in_isr = 1;
}

static void isr_exit( BaseType_t woken )
{
    fake_in_isr = 0;
    portYIELD_FROM_ISR( woken );
}

/* The queue family. Writes the op text; returns the result. */
static long queue_op( int cur_slot,
                      unsigned slot,
                      unsigned arg,
                      char * op,
                      size_t n )
{
    int q = ( int ) ( slot % QSLOTS );
    unsigned which = ( arg >> 8 ) % 14;
    uint32_t value = 10 + ( arg >> 16 ) % 90;
    uint32_t v = 0;
    BaseType_t woken = pdFALSE;
    long r = 0;

    if( queue[ q ] == NULL )
    {
        unsigned len = 1 + arg % 3;
        queue[ q ] = xQueueCreate( len, sizeof( uint32_t ) );
        queue_len[ q ] = len;
        snprintf( op, n, "qcreate %d %u", q, len );
        return queue[ q ] ? 1 : -1;
    }

    switch( which )
    {
        case 0:

            if( queue_has_waiter( q ) )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            vQueueDelete( queue[ q ] );
            queue[ q ] = NULL;
            snprintf( op, n, "qdelete %d", q );
            return 0;

        case 1:
        case 2:
        case 3:
        case 4:
        case 5:
        case 6:
        {
            /* The task-context calls, any of which may block. */
            static const enum call_kind kinds[] = { CALL_QSEND, CALL_QSENDF, CALL_QRECV, CALL_QRECV, CALL_QPEEK, CALL_QSEND };
            static const char * names[] = { "qsend", "qsendf", "qrecv", "qrecv", "qpeek", "qsend" };

            if( cur_slot < 0 )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            struct call c = { kinds[ which - 1 ], block_ticks( arg ), q, value };
            r = call_from_task( cur_slot, c );

            if( ( c.kind == CALL_QSEND ) || ( c.kind == CALL_QSENDF ) )
            {
                snprintf( op, n, "%s %d %lu %lu", names[ which - 1 ], q, ( unsigned long ) value, ( unsigned long ) c.ticks );
            }
            else
            {
                snprintf( op, n, "%s %d %lu", names[ which - 1 ], q, ( unsigned long ) c.ticks );
            }

            return r;
        }

        case 7:

            if( queue_len[ q ] != 1 )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            r = xQueueOverwrite( queue[ q ], &value );
            snprintf( op, n, "qover %d %lu", q, ( unsigned long ) value );
            return r;

        case 8:
            r = xQueueReset( queue[ q ] );
            snprintf( op, n, "qreset %d", q );
            return r;

        case 9:
            isr_enter();
            r = xQueueSendToBackFromISR( queue[ q ], &value, &woken );
            isr_exit( woken );
            snprintf( op, n, "qsend_isr %d %lu", q, ( unsigned long ) value );
            return r;

        case 10:
            isr_enter();
            r = xQueueSendToFrontFromISR( queue[ q ], &value, &woken );
            isr_exit( woken );
            snprintf( op, n, "qsendf_isr %d %lu", q, ( unsigned long ) value );
            return r;

        case 11:

            if( queue_len[ q ] != 1 )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            isr_enter();
            r = xQueueOverwriteFromISR( queue[ q ], &value, &woken );
            isr_exit( woken );
            snprintf( op, n, "qover_isr %d %lu", q, ( unsigned long ) value );
            return r;

        case 12:
            isr_enter();
            r = got( xQueueReceiveFromISR( queue[ q ], &v, &woken ), &v );
            isr_exit( woken );
            snprintf( op, n, "qrecv_isr %d", q );
            return r;

        default:
            isr_enter();

            if( arg & 1 )
            {
                r = got( xQueuePeekFromISR( queue[ q ], &v ), &v );
                snprintf( op, n, "qpeek_isr %d", q );
            }
            else
            {
                r = xQueueIsQueueFullFromISR( queue[ q ] );
                snprintf( op, n, "qfull_isr %d", q );
            }

            isr_exit( pdFALSE );
            return r;
    }
}

/* The mutex family: a mutex (priority inheritance), a recursive mutex and a
 * counting semaphore. A mutex is given only by its holder: from anyone else
 * the C's disinherit asserts, which is undefined behaviour to ask for. The
 * recursive give from a non-holder is defined (pdFAIL) and is asked for. */
static long mutex_op( int cur_slot,
                      unsigned arg,
                      char * op,
                      size_t n )
{
    unsigned which = ( arg >> 8 ) % 8;
    TickType_t ticks = block_ticks( arg );
    TaskHandle_t cur = ( cur_slot >= 0 ) ? app[ cur_slot ] : NULL;
    BaseType_t woken = pdFALSE;
    long r;

    if( ( which != 7 ) && ( cur_slot < 0 ) )
    {
        /* Every arm but the ISR's is made by a task. */
        snprintf( op, n, "noop" );
        return 0;
    }

    switch( which )
    {
        case 0:
        case 1:
        {
            /* Never from the holder: a task that takes a (non-recursive)
             * mutex it already holds, and times out, trips
             * vTaskPriorityDisinheritAfterTimeout's
             * `configASSERT( pxTCB != pxCurrentTCB )` -- the C calls it a
             * usage error. Found by this script's first run (step 6406).
             * The holder GIVES instead, which is also what keeps the mutex
             * changing hands: given only when its holder happened to be the
             * caller, it sat held for 19,208 of 20,000 steps. */
            if( xSemaphoreGetMutexHolder( mutex ) == cur )
            {
                r = xSemaphoreGive( mutex );
                snprintf( op, n, "mgive" );
                return r;
            }

            struct call c = { CALL_MTAKE, ticks, 0, 0 };
            r = call_from_task( cur_slot, c );
            snprintf( op, n, "mtake %lu", ( unsigned long ) ticks );
            return r;
        }

        case 2:

            if( xSemaphoreGetMutexHolder( mutex ) != cur )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            r = xSemaphoreGive( mutex );
            snprintf( op, n, "mgive" );
            return r;

        case 3:
        {
            struct call c = { CALL_RTAKE, ticks, 0, 0 };
            r = call_from_task( cur_slot, c );
            snprintf( op, n, "rtake %lu", ( unsigned long ) ticks );
            return r;
        }

        case 4:
            r = xSemaphoreGiveRecursive( rmutex );
            snprintf( op, n, "rgive" );
            return r;

        case 5:
        {
            struct call c = { CALL_CTAKE, ticks, 0, 0 };
            r = call_from_task( cur_slot, c );
            snprintf( op, n, "ctake %lu", ( unsigned long ) ticks );
            return r;
        }

        case 6:
            r = xSemaphoreGive( csem );
            snprintf( op, n, "cgive" );
            return r;

        default:
            isr_enter();
            r = xSemaphoreGiveFromISR( csem, &woken );
            isr_exit( woken );
            snprintf( op, n, "cgive_isr" );
            return r;
    }
}

/* ------------------------------------------------------- the script -- */

int main( int argc,
          char ** argv )
{
    char op[ 64 ];
    unsigned steps = ( argc > 2 ) ? ( unsigned ) strtoul( argv[ 2 ], NULL, 0 ) : 20000u;
    uint32_t seed = ( argc > 1 ) ? ( uint32_t ) strtoul( argv[ 1 ], NULL, 0 ) : 0x2545f491u;

    rng = seed;
    fake_yield_hook = on_yield;

    sem = xSemaphoreCreateBinary();
    mutex = xSemaphoreCreateMutex();
    rmutex = xSemaphoreCreateRecursiveMutex();
    csem = xSemaphoreCreateCounting( 3, 1 );
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
     * header says what it chose, and line 0 where it left the kernel. */
    printf( "seed=0x%08lx cores=%d steps=%u init=%lu,%lu,%lu,%lu\n",
            ( unsigned long ) seed, configNUMBER_OF_CORES, steps,
            ( unsigned long ) init[ 0 ], ( unsigned long ) init[ 1 ],
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

        if( kind < 6 )
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
        else if( kind < 11 )
        {
            if( ( t != NULL ) && !holds_a_mutex( t ) )
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
        else if( kind < 18 )
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
        else if( kind < 25 )
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
        else if( kind < 32 )
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
        else if( kind < 38 )
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
        else if( kind < 43 )
        {
            r = xSemaphoreGive( sem );
            snprintf( op, sizeof op, "give" );
        }
        else if( kind < 46 )
        {
            BaseType_t woken = pdFALSE;
            isr_enter();
            r = xSemaphoreGiveFromISR( sem, &woken );
            isr_exit( woken );
            snprintf( op, sizeof op, "give_isr" );
        }
        else if( kind < 51 )
        {
            if( cur_slot >= 0 )
            {
                struct call c = { CALL_TAKE, block_ticks( arg ), 0, 0 };
                r = call_from_task( cur_slot, c );
                snprintf( op, sizeof op, "take %lu", ( unsigned long ) c.ticks );
            }
            else
            {
                snprintf( op, sizeof op, "noop" );
            }
        }
        else if( kind < 75 )
        {
            r = queue_op( cur_slot, slot, arg, op, sizeof op );
        }
        else if( kind < 87 )
        {
            r = mutex_op( cur_slot, arg, op, sizeof op );
        }
        else if( kind < 97 )
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
