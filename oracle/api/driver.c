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
 * create that failed, -2 for "blocked, now pending" (a notification count can be 2), and a RECEIVED VALUE
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
#include "event_groups.h"
#include "stream_buffer.h"
#include "message_buffer.h"

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
#define GSLOTS    2                /* event groups */
static EventGroupHandle_t group[ GSLOTS ];
/* Buffer 0 is a stream buffer of 12 bytes, buffer 1 a message buffer of 20. */
#define BSLOTS    2
#define SB_SIZE   12
#define MB_SIZE   20
static StreamBufferHandle_t buffer[ BSLOTS ];
#define TSLOTS    3 /* software timers */
static TimerHandle_t tmr[ TSLOTS ];
/* Each slot's callbacks, and (last) those of a timer whose delete was
 * still queued when it fired. */
static unsigned long fired[ TSLOTS + 1 ];
/* What the pended function has been handed, summed. */
static unsigned long pended_sum;
/* Callbacks and pended functions the daemon ran: a `daemon` step's result. */
static unsigned long daemon_work;
/* Event-group ISR calls posted to the daemon and not yet run: a group with
 * one outstanding is not deleted (the daemon would set bits in freed
 * memory). The daemon drains its queue before it blocks, so every `daemon`
 * step that ends clears these. */
static unsigned group_pend_out[ GSLOTS ];
static TaskHandle_t daemon;
static unsigned queue_len[ QSLOTS ];
/* One queue set, room for every member's every item (lengths are 1..3). A
 * member is never received from directly, reset or deleted, and is added
 * only while no task waits on it: each of those can leave the set holding an
 * entry for an item that is gone, and enough of them overflow it -- the C
 * asserts on that (prvNotifyQueueSetContainer). */
#define SET_LEN    ( 3 * QSLOTS )
static QueueSetHandle_t qset;
static int in_set[ QSLOTS ];
/* The last task deleted while it was running on a core: its TCB waits on
 * the termination list for an idle task that never runs here, so its handle
 * stays valid and eTaskGetState answers eDeleted. (One deleted while NOT
 * running is freed at once, and must never be asked about.) */
static TaskHandle_t dead;
/* Each task's `pxPreviousWakeTime` for xTaskDelayUntil. */
static TickType_t wake[ SLOTS ];

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

enum call_kind { CALL_TAKE, CALL_QSEND, CALL_QSENDF, CALL_QRECV, CALL_QPEEK, CALL_MTAKE, CALL_RTAKE, CALL_CTAKE, CALL_NTAKE, CALL_NWAIT, CALL_GWAIT, CALL_GSYNC, CALL_BSEND, CALL_BRECV, CALL_TCMD, CALL_TPEND, CALL_SSELECT };

/* A timer's callback: count it against the slot holding the timer. */
static void timer_cb( TimerHandle_t t )
{
    int i = 0;

    while( ( i < TSLOTS ) && ( tmr[ i ] != t ) )
    {
        i++;
    }

    fired[ i ]++;
    daemon_work++;
}

/* What `xTimerPendFunctionCall` defers: sum its second parameter. */
/* A select's result: 0 for none, else (the member's slot + 1) * 100 plus the
 * value its receive took -- the select and the receive it licenses are one
 * step, as the C documents them. */
static long select_and_take( QueueSetMemberHandle_t m,
                             int from_isr )
{
    uint32_t v = 0;
    int q = 0;
    BaseType_t woken = pdFALSE;
    BaseType_t ok;

    if( m == NULL )
    {
        return 0;
    }

    while( ( q < QSLOTS ) && ( queue[ q ] != m ) )
    {
        q++;
    }

    configASSERT( q < QSLOTS );
    ok = from_isr ? xQueueReceiveFromISR( m, &v, &woken ) : xQueueReceive( m, &v, 0 );
    ( void ) woken; /* a sender it wakes is left a PENDING yield, as an ISR
                     * with no woken pointer leaves one */
    return ( long ) ( q + 1 ) * 100 + ( ok ? ( long ) v : 0 );
}

static void pended_fn( void * p1,
                       uint32_t p2 )
{
    ( void ) p1;
    pended_sum += p2;
    daemon_work++;
}

/* A task's timer command: 0 start, 1 stop, 2 reset, 3 change period. */
static long timer_cmd( int t,
                       uint32_t cmd,
                       uint32_t value,
                       TickType_t ticks )
{
    switch( cmd )
    {
        case 0:  return xTimerStart( tmr[ t ], ticks );
        case 1:  return xTimerStop( tmr[ t ], ticks );
        case 2:  return xTimerReset( tmr[ t ], ticks );
        default: return xTimerChangePeriod( tmr[ t ], value, ticks );
    }
}

/* Send `len` bytes counting up from `start` -- the replay builds the same
 * bytes from the same two numbers. */
static long buffer_send( int b,
                         unsigned len,
                         unsigned start,
                         TickType_t ticks )
{
    uint8_t data[ 32 ];

    for( unsigned i = 0; i < len; i++ )
    {
        data[ i ] = ( uint8_t ) ( start + i );
    }

    return ( long ) xStreamBufferSend( buffer[ b ], data, len, ticks );
}

/* A receive's result: how many bytes, times 100000, plus their sum -- a
 * wrong byte shows as surely as a wrong count. */
static long fold( size_t n,
                  const uint8_t * data )
{
    long sum = 0;

    for( size_t i = 0; i < n; i++ )
    {
        sum += data[ i ];
    }

    return ( long ) n * 100000 + sum;
}

static long buffer_recv( int b,
                         unsigned max,
                         TickType_t ticks )
{
    uint8_t data[ 32 ];
    size_t n = xStreamBufferReceive( buffer[ b ], data, max, ticks );

    return fold( n, data );
}

static int is_group_call( enum call_kind k )
{
    return ( k == CALL_GWAIT ) || ( k == CALL_GSYNC );
}

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
    uint32_t a;     /* a notify take's clear flag, or a wait's entry mask */
    uint32_t b;     /* a notify wait's exit mask */
};

/* The timer daemon is coroutine SLOTS: the C runs `prvTimerTask` itself. */
#define DAEMON    SLOTS
extern TaskFunction_t fake_last_code;
extern void * fake_last_param;
static TaskFunction_t daemon_code;
static void * daemon_param;
static int daemon_started;

#define CORO_STACK    ( 256 * 1024 )
static ucontext_t driver_ctx;
static ucontext_t coro[ SLOTS + 1 ];
static char * coro_stack[ SLOTS + 1 ];
static struct call coro_call[ SLOTS ];
static int pending[ SLOTS ];
static int coro_done[ SLOTS ];
static long coro_result[ SLOTS ];
static int in_coro = -1;
static int coro_start_slot;

extern volatile int fake_suspended;

/* A yield of the core the coroutine runs on: if the call has just blocked,
 * leave the kernel here. Any other yield (a preemption, or one taken while
 * the scheduler is suspended, which a real switch declines) is only
 * recorded. */
static void on_yield( void )
{
    if( ( in_coro >= 0 ) && fake_blocking && ( fake_suspended == 0 ) )
    {
        fake_blocking = 0;
        swapcontext( &coro[ in_coro ], &driver_ctx );
    }
}

/* xTaskNotifyWait's result: its pdTRUE / pdFALSE, plus TWICE the value it
 * received -- one number, and a value of 0 with success still reads 1. */
static long notify_wait( uint32_t entry,
                         uint32_t exit,
                         TickType_t ticks )
{
    uint32_t nv = 0;
    BaseType_t ok = xTaskNotifyWait( entry, exit, &nv, ticks );

    return ( long ) ok + 2 * ( long ) nv;
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

        case CALL_NTAKE:
            coro_result[ s ] = ( long ) ulTaskNotifyTake( c->a, c->ticks );
            break;

        case CALL_NWAIT:
            coro_result[ s ] = notify_wait( c->a, c->b, c->ticks );
            break;

        case CALL_GWAIT:
            coro_result[ s ] = ( long ) xEventGroupWaitBits( group[ c->q ], c->a, c->b & 1, ( c->b >> 1 ) & 1, c->ticks );
            break;

        case CALL_GSYNC:
            coro_result[ s ] = ( long ) xEventGroupSync( group[ c->q ], c->value, c->a, c->ticks );
            break;

        case CALL_BSEND:
            coro_result[ s ] = buffer_send( c->q, c->a, c->value, c->ticks );
            break;

        case CALL_BRECV:
            coro_result[ s ] = buffer_recv( c->q, c->a, c->ticks );
            break;

        case CALL_TCMD:
            coro_result[ s ] = timer_cmd( c->q, c->a, c->value, c->ticks );
            break;

        case CALL_TPEND:
            coro_result[ s ] = xTimerPendFunctionCall( pended_fn, NULL, c->value, c->ticks );
            break;

        case CALL_SSELECT:
            coro_result[ s ] = select_and_take( xQueueSelectFromSet( qset, c->ticks ), 0 );
            break;
    }

    coro_done[ s ] = 1;
    /* returning resumes uc_link: the driver */
}

/* Run slot s's coroutine until it completes or blocks again: the call's
 * result, or -2 for "blocked". */
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
    return -2;
}

static void daemon_main( void )
{
    daemon_code( daemon_param ); /* never returns */
}

/* Run the timer daemon until it blocks again: the callbacks and pended
 * functions it ran. It is never preempted -- it has the top priority -- so
 * the only yield that leaves it is the one after it blocks. */
static long run_daemon( void )
{
    unsigned long before = daemon_work;

    if( !daemon_started )
    {
        coro_stack[ DAEMON ] = malloc( CORO_STACK );
        getcontext( &coro[ DAEMON ] );
        coro[ DAEMON ].uc_stack.ss_sp = coro_stack[ DAEMON ];
        coro[ DAEMON ].uc_stack.ss_size = CORO_STACK;
        coro[ DAEMON ].uc_link = &driver_ctx;
        makecontext( &coro[ DAEMON ], daemon_main, 0 );
        daemon_started = 1;
    }

    in_coro = DAEMON;
    fake_blocking = 0;
    swapcontext( &driver_ctx, &coro[ DAEMON ] );
    in_coro = -1;

    for( int g = 0; g < GSLOTS; g++ )
    {
        group_pend_out[ g ] = 0;
    }

    return ( long ) ( daemon_work - before );
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

/* The same rule for an event group: no deleting one a task waits on. */
static int group_has_waiter( int g )
{
    for( int i = 0; i < SLOTS; i++ )
    {
        if( pending[ i ] && is_group_call( coro_call[ i ].kind ) && ( coro_call[ i ].q == g ) )
        {
            return 1;
        }
    }

    return 0;
}

/* Whether a task is blocked in a `kind` call on buffer b. A stream buffer
 * has ONE reader and ONE writer: the C asserts if a second task blocks on
 * the same side, so the script never asks. */
static int buffer_has_waiter( int b,
                              int kind )
{
    for( int i = 0; i < SLOTS; i++ )
    {
        if( pending[ i ] && ( ( int ) coro_call[ i ].kind == kind ) && ( coro_call[ i ].q == b ) )
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

    printf( " M=%s R=%s C=%lu N=", name_of( xSemaphoreGetMutexHolder( mutex ) ),
            name_of( xSemaphoreGetMutexHolder( rmutex ) ),
            ( unsigned long ) uxSemaphoreGetCount( csem ) );

    /* Every app task's notification value: `ulTaskNotifyValueClear` with no
     * bits to clear is the C's only read of it that changes nothing. */
    for( int i = 0; i < SLOTS; i++ )
    {
        if( app[ i ] == NULL )
        {
            printf( "-" );
        }
        else
        {
            printf( "%lu", ( unsigned long ) ulTaskNotifyValueClear( app[ i ], 0 ) );
        }

        printf( i + 1 < SLOTS ? "." : "" );
    }

    /* A debugging aid, off unless asked for: KAIROS_API_DEBUG=<step> writes
     * every pending call (task slot, kind, object) to stderr at that step. */
    {
        static long debug_at = -2;

        if( debug_at == -2 )
        {
            const char * e = getenv( "KAIROS_API_DEBUG" );
            debug_at = e ? strtol( e, NULL, 0 ) : -1;
        }

        if( ( long ) step == debug_at )
        {
            for( int i = 0; i < SLOTS; i++ )
            {
                if( pending[ i ] )
                {
                    fprintf( stderr, "step %u pending: slot %d (%s) kind %d obj %d a=%lu b=%lu value=%lu ticks=%lu\n",
                             step, i, name_of( app[ i ] ), ( int ) coro_call[ i ].kind, coro_call[ i ].q,
                             ( unsigned long ) coro_call[ i ].a, ( unsigned long ) coro_call[ i ].b,
                             ( unsigned long ) coro_call[ i ].value, ( unsigned long ) coro_call[ i ].ticks );
                }
            }
        }
    }

    printf( " E=" );

    for( int g = 0; g < GSLOTS; g++ )
    {
        if( group[ g ] == NULL )
        {
            printf( "-" );
        }
        else
        {
            printf( "%lu", ( unsigned long ) xEventGroupGetBits( group[ g ] ) );
        }

        printf( g + 1 < GSLOTS ? "." : "" );
    }

    printf( " B=" );

    for( int b = 0; b < BSLOTS; b++ )
    {
        if( buffer[ b ] == NULL )
        {
            printf( "-" );
        }
        else
        {
            printf( "%lu", ( unsigned long ) xStreamBufferBytesAvailable( buffer[ b ] ) );
        }

        printf( b + 1 < BSLOTS ? "." : "" );
    }

    /* Each timer's callbacks and whether it is active, then the orphans;
     * then the pended functions' sum. */
    printf( " F=" );

    for( int t = 0; t < TSLOTS; t++ )
    {
        if( tmr[ t ] == NULL )
        {
            printf( "-" );
        }
        else
        {
            printf( "%lu%c", fired[ t ], xTimerIsTimerActive( tmr[ t ] ) ? 'a' : 'i' );
        }

        printf( t + 1 < TSLOTS ? "." : "" );
    }

    printf( "/%lu P=%lu S=%lu", fired[ TSLOTS ], pended_sum, ( unsigned long ) uxQueueMessagesWaiting( qset ) );

    printf( "\n" );
}

/* Whether task t holds either mutex: such a task is not deleted (the C would
 * leave the mutex naming a freed TCB). */
static int holds_a_mutex( TaskHandle_t t )
{
    /* The FromISR read: no critical section. A guard must not touch the
     * kernel in a way the replay does not -- on two cores every task-level
     * exit is a yield point (`vTaskExitCritical`). */
    return ( xSemaphoreGetMutexHolderFromISR( mutex ) == t ) || ( xSemaphoreGetMutexHolderFromISR( rmutex ) == t );
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
        case CALL_NTAKE:  return ( long ) ulTaskNotifyTake( c.a, 0 );
        case CALL_NWAIT:  return notify_wait( c.a, c.b, 0 );
        case CALL_GWAIT:  return ( long ) xEventGroupWaitBits( group[ c.q ], c.a, c.b & 1, ( c.b >> 1 ) & 1, 0 );
        case CALL_GSYNC:  return ( long ) xEventGroupSync( group[ c.q ], c.value, c.a, 0 );
        case CALL_BSEND:  return buffer_send( c.q, c.a, c.value, 0 );
        case CALL_BRECV:  return buffer_recv( c.q, c.a, 0 );
        case CALL_TCMD:   return timer_cmd( c.q, c.a, c.value, 0 );
        case CALL_TPEND:  return xTimerPendFunctionCall( pended_fn, NULL, c.value, 0 );
        case CALL_SSELECT: return select_and_take( xQueueSelectFromSet( qset, 0 ), 0 );
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

            if( queue_has_waiter( q ) || in_set[ q ] )
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

            /* A set member is read only through the set. */
            if( ( cur_slot < 0 ) || ( in_set[ q ] && ( kinds[ which - 1 ] == CALL_QRECV ) ) )
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

            if( in_set[ q ] )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

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

            if( in_set[ q ] )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

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
            if( xSemaphoreGetMutexHolderFromISR( mutex ) == cur )
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

            if( xSemaphoreGetMutexHolderFromISR( mutex ) != cur )
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

/* The notification family, index 0 (configTASK_NOTIFICATION_ARRAY_ENTRIES
 * is 1). Actions are printed as the C enum's numbers: 0 eNoAction, 1
 * eSetBits, 2 eIncrement, 3 eSetValueWithOverwrite, 4
 * eSetValueWithoutOverwrite. Values stay small so increments stay legible. */
static long notify_op( int cur_slot,
                       unsigned slot,
                       unsigned arg,
                       char * op,
                       size_t n )
{
    unsigned which = ( arg >> 8 ) % 9;
    TaskHandle_t t = app[ slot ];
    eNotifyAction action = ( eNotifyAction ) ( ( arg >> 4 ) % 5 );
    uint32_t value = ( arg >> 12 ) % 16;
    TickType_t ticks = block_ticks( arg );
    BaseType_t woken = pdFALSE;
    uint32_t prev = 0;
    long r;

    if( which <= 4 )
    {
        /* Aimed at a task: there has to be one. */
        if( t == NULL )
        {
            snprintf( op, n, "noop" );
            return 0;
        }
    }
    else if( which <= 6 )
    {
        /* Made BY a task. */
        if( cur_slot < 0 )
        {
            snprintf( op, n, "noop" );
            return 0;
        }
    }

    switch( which )
    {
        case 0:
            r = xTaskNotify( t, value, action );
            snprintf( op, n, "ntf %u %d %lu", slot, ( int ) action, ( unsigned long ) value );
            return r;

        case 1:
            r = xTaskNotifyAndQuery( t, value, action, &prev );
            snprintf( op, n, "ntfq %u %d %lu", slot, ( int ) action, ( unsigned long ) value );
            return r + 2 * ( long ) prev;

        case 2:
            isr_enter();
            r = xTaskNotifyFromISR( t, value, action, &woken );
            isr_exit( woken );
            snprintf( op, n, "ntf_isr %u %d %lu", slot, ( int ) action, ( unsigned long ) value );
            return r;

        case 3:
            isr_enter();
            r = xTaskNotifyAndQueryFromISR( t, value, action, &prev, &woken );
            isr_exit( woken );
            snprintf( op, n, "ntfq_isr %u %d %lu", slot, ( int ) action, ( unsigned long ) value );
            return r + 2 * ( long ) prev;

        case 4:
            isr_enter();
            vTaskNotifyGiveFromISR( t, &woken );
            isr_exit( woken );
            snprintf( op, n, "ngive_isr %u", slot );
            return 0;

        case 5:
        {
            struct call c = { CALL_NTAKE, ticks, 0, 0, arg & 1, 0 };
            r = call_from_task( cur_slot, c );
            snprintf( op, n, "ntake %lu %lu", ( unsigned long ) c.a, ( unsigned long ) ticks );
            return r;
        }

        case 6:
        {
            struct call c = { CALL_NWAIT, ticks, 0, 0, ( arg >> 1 ) % 4, ( arg >> 3 ) % 4 };
            r = call_from_task( cur_slot, c );
            snprintf( op, n, "nwait %lu %lu %lu", ( unsigned long ) c.a, ( unsigned long ) c.b, ( unsigned long ) ticks );
            return r;
        }

        case 7:

            if( t == NULL )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            r = xTaskNotifyStateClear( t );
            snprintf( op, n, "nstate_clear %u", slot );
            return r;

        default:

            if( t == NULL )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            r = ( long ) ulTaskNotifyValueClear( t, value );
            snprintf( op, n, "nvalue_clear %u %lu", slot, ( unsigned long ) value );
            return r;
    }
}

/* The event-group family. The ISR set and clear are deferred to the timer
 * daemon (`xTimerPendFunctionCallFromISR`), so they also fail when its
 * one-slot queue is full. Bits stay in the low four; a wait mask is never
 * zero (the C asserts it). */
static long group_op( int cur_slot,
                      unsigned slot,
                      unsigned arg,
                      char * op,
                      size_t n )
{
    int g = ( int ) ( slot % GSLOTS );
    unsigned which = ( arg >> 8 ) % 11;
    EventBits_t bits = ( arg >> 12 ) % 16;
    EventBits_t mask = 1 + ( arg >> 16 ) % 15;
    TickType_t ticks = block_ticks( arg );
    long r;

    if( group[ g ] == NULL )
    {
        group[ g ] = xEventGroupCreate();
        snprintf( op, n, "gcreate %d", g );
        return group[ g ] ? 1 : -1;
    }

    switch( which )
    {
        case 0:

            if( group_has_waiter( g ) || group_pend_out[ g ] )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            vEventGroupDelete( group[ g ] );
            group[ g ] = NULL;
            snprintf( op, n, "gdelete %d", g );
            return 0;

        case 1:
        case 2:
            r = ( long ) xEventGroupSetBits( group[ g ], bits );
            snprintf( op, n, "gset %d %lu", g, ( unsigned long ) bits );
            return r;

        case 3:
            r = ( long ) xEventGroupClearBits( group[ g ], bits );
            snprintf( op, n, "gclear %d %lu", g, ( unsigned long ) bits );
            return r;

        case 4:
            isr_enter();
            r = ( long ) xEventGroupGetBitsFromISR( group[ g ] );
            isr_exit( pdFALSE );
            snprintf( op, n, "gget_isr %d", g );
            return r;

        case 5:
        case 6:
        case 7:
        {
            if( cur_slot < 0 )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            unsigned flags = ( arg >> 4 ) % 4; /* bit 0 clear on exit, bit 1 wait for all */
            struct call c = { CALL_GWAIT, ticks, g, 0, mask, flags };
            r = call_from_task( cur_slot, c );
            snprintf( op, n, "gwait %d %lu %u %lu", g, ( unsigned long ) mask, flags, ( unsigned long ) ticks );
            return r;
        }

        case 9:
        case 10:
        {
            BaseType_t woken = pdFALSE;
            isr_enter();
            r = ( which == 9 ) ? xEventGroupSetBitsFromISR( group[ g ], bits, &woken )
                : xEventGroupClearBitsFromISR( group[ g ], bits );
            isr_exit( woken );

            if( r )
            {
                group_pend_out[ g ]++;
            }

            snprintf( op, n, "%s %d %lu", ( which == 9 ) ? "gset_isr" : "gclear_isr", g, ( unsigned long ) bits );
            return r;
        }

        default:
        {
            if( cur_slot < 0 )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            struct call c = { CALL_GSYNC, ticks, g, bits, mask, 0 };
            r = call_from_task( cur_slot, c );
            snprintf( op, n, "gsync %d %lu %lu %lu", g, ( unsigned long ) bits, ( unsigned long ) mask, ( unsigned long ) ticks );
            return r;
        }
    }
}

/* The scheduling odds and ends: abort a delay, delay until, look a task up
 * by name, suspend the scheduler around a tick or a give, an empty critical
 * section (on two cores every task-level exit is a yield point), and the
 * tick count both ways. */
static long sched_op( int cur_slot,
                      unsigned slot,
                      unsigned arg,
                      char * op,
                      size_t n )
{
    unsigned which = ( arg >> 8 ) % 10;
    TaskHandle_t t = app[ slot ];
    BaseType_t woken = pdFALSE;
    long r = 0;

    switch( which )
    {
        case 0:

            if( t == NULL )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            r = xTaskAbortDelay( t );
            snprintf( op, n, "abort %u", slot );
            return r;

        case 1:
        {
            if( cur_slot < 0 )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            TickType_t inc = 1 + ( arg >> 12 ) % 5;
            TickType_t prev = wake[ cur_slot ];
            BaseType_t ok = xTaskDelayUntil( &prev, inc );
            wake[ cur_slot ] = prev;
            snprintf( op, n, "dlyuntil %lu", ( unsigned long ) inc );
            return ( long ) ok + 2 * ( long ) prev;
        }

        case 2:
        {
            /* A live task's own name, or one no task ever had. A deleted
             * task's name is not asked: the C keeps a self-deleted TCB on
             * its termination list, which its idle task never empties here. */
            const char * name = t ? pcTaskGetName( t ) : "tX";
            TaskHandle_t h = xTaskGetHandle( name );
            snprintf( op, n, "gethandle %u %s", slot, name );
            return ( h == NULL ) ? 0 : ( h == t ) ? 1 : 2;
        }

        case 3:
        {
            unsigned sub = ( arg >> 12 ) % 3;
            long a;

            vTaskSuspendAll();

            if( sub == 0 )
            {
                fake_in_isr = 1;
                UBaseType_t saved = taskENTER_CRITICAL_FROM_ISR();
                a = xTaskIncrementTick();
                taskEXIT_CRITICAL_FROM_ISR( saved );
                fake_in_isr = 0;

                if( a )
                {
                    portYIELD();
                }
            }
            else if( sub == 1 )
            {
                a = xSemaphoreGive( sem );
            }
            else
            {
                isr_enter();
                a = xSemaphoreGiveFromISR( sem, &woken );
                isr_exit( woken );
            }

            r = 2 * a + xTaskResumeAll();
            snprintf( op, n, "sall %u", sub );
            return r;
        }

        case 4:
            taskENTER_CRITICAL();
            taskEXIT_CRITICAL();
            snprintf( op, n, "crit" );
            return 0;

        case 5:
            r = ( long ) xTaskGetTickCount();
            snprintf( op, n, "tickcount" );
            return r;

        case 6:
            isr_enter();
            r = ( long ) xTaskGetTickCountFromISR();
            isr_exit( pdFALSE );
            snprintf( op, n, "tickcount_isr" );
            return r;

        case 7:
            r = ( long ) uxQueueSpacesAvailable( sem );
            snprintf( op, n, "semspaces" );
            return r;

        case 8:
        {
            /* vTaskStepTick as a tickless port calls it: scheduler suspended,
             * one tick. Never from 0xFFFFFFFF -- a step does not swap the
             * delayed lists, so stepping across the wrap is undefined in both
             * kernels -- and one tick never passes the next unblock time. */
            TickType_t now = xTaskGetTickCount();

            if( now != ( TickType_t ) 0xFFFFFFFFUL )
            {
                vTaskSuspendAll();
                vTaskStepTick( 1 );
                r = 1 + 2 * ( long ) xTaskResumeAll();
            }

            snprintf( op, n, "steptick" );
            return r;
        }

        default:

            if( dead == NULL )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            r = ( long ) eTaskGetState( dead );
            snprintf( op, n, "deadstate" );
            return r;
    }
}

/* The queue-set family. See SET_LEN for what a member may not do. */
static long set_op( int cur_slot,
                    unsigned slot,
                    unsigned arg,
                    char * op,
                    size_t n )
{
    int q = ( int ) ( slot % QSLOTS );
    unsigned which = ( arg >> 8 ) % 6;
    TickType_t ticks = block_ticks( arg >> 12 );
    BaseType_t woken = pdFALSE;
    long r;

    switch( which )
    {
        case 0:

            if( ( queue[ q ] == NULL ) || in_set[ q ] || queue_has_waiter( q ) )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            r = xQueueAddToSet( queue[ q ], qset );
            in_set[ q ] = ( r == pdPASS );
            snprintf( op, n, "sadd %d", q );
            return r;

        case 1:

            if( !in_set[ q ] )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            r = xQueueRemoveFromSet( queue[ q ], qset );
            in_set[ q ] = ( r != pdPASS );
            snprintf( op, n, "sremove %d", q );
            return r;

        case 2:
        case 3:
        {
            if( cur_slot < 0 )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            struct call c = { CALL_SSELECT, ticks, 0, 0, 0, 0 };
            r = call_from_task( cur_slot, c );
            snprintf( op, n, "sselect %lu", ( unsigned long ) ticks );
            return r;
        }

        case 4:
            isr_enter();
            r = select_and_take( xQueueSelectFromSetFromISR( qset ), 1 );
            isr_exit( woken );
            snprintf( op, n, "sselect_isr" );
            return r;

        default:

            if( queue[ q ] == NULL )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            r = ( long ) uxQueueSpacesAvailable( queue[ q ] );
            snprintf( op, n, "qspaces %d", q );
            return r;
    }
}

/* Whether a task is blocked sending a command for timer slot t: such a
 * timer is not deleted (the command would name a freed timer). */
static int timer_has_sender( int t )
{
    for( int i = 0; i < SLOTS; i++ )
    {
        if( pending[ i ] && ( coro_call[ i ].kind == CALL_TCMD ) && ( coro_call[ i ].q == t ) )
        {
            return 1;
        }
    }

    return 0;
}

/* The timer family. Periods are 1..6 ticks. A delete posts its command and
 * forgets the handle at once: the daemon frees the timer, so nothing may
 * name it afterwards. Expiry is read only while active -- a timer never
 * started has an item value nothing ever wrote. */
static long timer_op( int cur_slot,
                      unsigned slot,
                      unsigned arg,
                      char * op,
                      size_t n )
{
    int t = ( int ) ( slot % TSLOTS );
    unsigned which = ( arg >> 8 ) % 17;
    uint32_t period = 1 + ( arg >> 4 ) % 6;
    uint32_t value = ( arg >> 12 ) % 10;
    TickType_t ticks = block_ticks( arg >> 20 );
    BaseType_t woken = pdFALSE;
    long r;

    if( ( which == 15 ) || ( which == 16 ) )
    {
        if( which == 15 )
        {
            if( cur_slot < 0 )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            struct call c = { CALL_TPEND, ticks, 0, value, 0, 0 };
            r = call_from_task( cur_slot, c );
            snprintf( op, n, "tpend %lu %lu", ( unsigned long ) value, ( unsigned long ) ticks );
            return r;
        }

        isr_enter();
        r = xTimerPendFunctionCallFromISR( pended_fn, NULL, value, &woken );
        isr_exit( woken );
        snprintf( op, n, "tpend_isr %lu", ( unsigned long ) value );
        return r;
    }

    if( tmr[ t ] == NULL )
    {
        UBaseType_t reload = ( arg >> 16 ) & 1;
        tmr[ t ] = xTimerCreate( "T", period, reload, ( void * ) ( uintptr_t ) t, timer_cb );
        snprintf( op, n, "tcreate %d %lu %lu", t, ( unsigned long ) period, ( unsigned long ) reload );
        return tmr[ t ] ? 1 : -1;
    }

    switch( which )
    {
        case 0:

            if( timer_has_sender( t ) )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            r = xTimerDelete( tmr[ t ], 0 );

            if( r )
            {
                tmr[ t ] = NULL;
            }

            snprintf( op, n, "tdelete %d", t );
            return r;

        case 1:
        case 2:
        case 3:
        case 4:
        case 5:
        {
            static const uint32_t cmds[] = { 0, 0, 1, 2, 3 };
            static const char * names[] = { "tstart", "tstart", "tstop", "treset", "tperiod" };

            if( cur_slot < 0 )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            struct call c = { CALL_TCMD, ticks, t, period, cmds[ which - 1 ], 0 };
            r = call_from_task( cur_slot, c );
            snprintf( op, n, "%s %d %lu %lu", names[ which - 1 ], t, ( unsigned long ) period, ( unsigned long ) ticks );
            return r;
        }

        case 6:
        case 7:
        case 8:
        case 9:
            isr_enter();

            switch( which )
            {
                case 6:  r = xTimerStartFromISR( tmr[ t ], &woken ); break;
                case 7:  r = xTimerStopFromISR( tmr[ t ], &woken ); break;
                case 8:  r = xTimerResetFromISR( tmr[ t ], &woken ); break;
                default: r = xTimerChangePeriodFromISR( tmr[ t ], period, &woken ); break;
            }

            isr_exit( woken );
            snprintf( op, n, "%s %d %lu", ( which == 6 ) ? "tstart_isr" : ( which == 7 ) ? "tstop_isr" : ( which == 8 ) ? "treset_isr" : "tperiod_isr",
                      t, ( unsigned long ) period );
            return r;

        case 10:
            r = xTimerIsTimerActive( tmr[ t ] );
            snprintf( op, n, "tactive %d", t );
            return r;

        case 11:
            r = ( long ) xTimerGetPeriod( tmr[ t ] ) + 100 * ( long ) uxTimerGetReloadMode( tmr[ t ] ) +
                1000 * ( long ) ( uintptr_t ) pvTimerGetTimerID( tmr[ t ] );
            snprintf( op, n, "tinfo %d", t );
            return r;

        case 12:
            vTimerSetReloadMode( tmr[ t ], value & 1 );
            snprintf( op, n, "treload %d %lu", t, ( unsigned long ) ( value & 1 ) );
            return 0;

        case 13:
            vTimerSetTimerID( tmr[ t ], ( void * ) ( uintptr_t ) value );
            snprintf( op, n, "tsetid %d %lu", t, ( unsigned long ) value );
            return 0;

        default:

            /* A real call, so a real op: the replay makes it too. */
            if( !xTimerIsTimerActive( tmr[ t ] ) )
            {
                snprintf( op, n, "tinactive %d", t );
                return 0;
            }

            r = ( long ) xTimerGetExpiryTime( tmr[ t ] );
            snprintf( op, n, "texpiry %d", t );
            return r;
    }
}

/* The buffer family: buffer 0 a stream buffer (12 bytes, trigger level
 * 1..3), buffer 1 a message buffer (20 bytes; each message also costs
 * sizeof( size_t ) of length prefix -- eight here). */
static long buffer_op( int cur_slot,
                       unsigned slot,
                       unsigned arg,
                       char * op,
                       size_t n )
{
    int b = ( int ) ( slot % BSLOTS );
    unsigned which = ( arg >> 8 ) % 13;
    /* One stream send in eight is longer than the whole buffer: the send
     * clamps it to what the buffer can ever report free. */
    unsigned len = ( ( b == 0 ) && ( ( arg >> 20 ) % 8 == 0 ) ) ? SB_SIZE + 1 : 1 + ( arg >> 12 ) % ( b == 0 ? 6 : 5 );
    unsigned start = ( arg >> 16 ) & 0xff;
    TickType_t ticks = block_ticks( arg );
    BaseType_t woken = pdFALSE;
    long r;

    if( buffer[ b ] == NULL )
    {
        if( b == 0 )
        {
            unsigned trigger = 1 + arg % 3;
            buffer[ b ] = xStreamBufferCreate( SB_SIZE, trigger );
            snprintf( op, n, "bcreate %d %u", b, trigger );
        }
        else
        {
            buffer[ b ] = xMessageBufferCreate( MB_SIZE );
            snprintf( op, n, "bcreate %d 0", b );
        }

        return buffer[ b ] ? 1 : -1;
    }

    switch( which )
    {
        case 0:

            if( buffer_has_waiter( b, CALL_BSEND ) || buffer_has_waiter( b, CALL_BRECV ) )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            vStreamBufferDelete( buffer[ b ] );
            buffer[ b ] = NULL;
            snprintf( op, n, "bdelete %d", b );
            return 0;

        case 1:
        case 2:
        {
            if( ( cur_slot < 0 ) || buffer_has_waiter( b, CALL_BSEND ) )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            struct call c = { CALL_BSEND, ticks, b, start, len, 0 };
            r = call_from_task( cur_slot, c );
            snprintf( op, n, "bsend %d %u %u %lu", b, len, start, ( unsigned long ) ticks );
            return r;
        }

        case 3:
        case 4:
        {
            if( ( cur_slot < 0 ) || buffer_has_waiter( b, CALL_BRECV ) )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            unsigned max = 1 + ( arg >> 12 ) % 8;
            struct call c = { CALL_BRECV, ticks, b, 0, max, 0 };
            r = call_from_task( cur_slot, c );
            snprintf( op, n, "brecv %d %u %lu", b, max, ( unsigned long ) ticks );
            return r;
        }

        case 5:
        {
            uint8_t data[ 32 ];

            /* Single writer: a task blocked sending holds a stale xSpace,
             * and a second writer under it makes the C overwrite. */
            if( buffer_has_waiter( b, CALL_BSEND ) )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            for( unsigned i = 0; i < len; i++ )
            {
                data[ i ] = ( uint8_t ) ( start + i );
            }

            isr_enter();
            r = ( long ) xStreamBufferSendFromISR( buffer[ b ], data, len, &woken );
            isr_exit( woken );
            snprintf( op, n, "bsend_isr %d %u %u", b, len, start );
            return r;
        }

        case 6:
        {
            uint8_t data[ 32 ];
            unsigned max = 1 + ( arg >> 12 ) % 8;

            /* Single reader, the same contract from the other side. */
            if( buffer_has_waiter( b, CALL_BRECV ) )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            isr_enter();
            size_t got_n = xStreamBufferReceiveFromISR( buffer[ b ], data, max, &woken );
            isr_exit( woken );
            snprintf( op, n, "brecv_isr %d %u", b, max );
            return fold( got_n, data );
        }

        case 7:
            r = ( long ) xStreamBufferSpacesAvailable( buffer[ b ] );
            snprintf( op, n, "bspace %d", b );
            return r;

        case 8:
            r = xStreamBufferIsFull( buffer[ b ] ) + 2 * xStreamBufferIsEmpty( buffer[ b ] );
            snprintf( op, n, "bfullempty %d", b );
            return r;

        case 9:
            r = ( long ) xStreamBufferNextMessageLengthBytes( buffer[ b ] );
            snprintf( op, n, "bnext %d", b );
            return r;

        case 10:

            if( b != 0 )
            {
                snprintf( op, n, "noop" );
                return 0;
            }

            {
                unsigned level = arg % 15; /* 0 becomes 1; past SB_SIZE: refused */
                r = xStreamBufferSetTriggerLevel( buffer[ b ], level );
                snprintf( op, n, "btrigger %d %u", b, level );
                return r;
            }

        case 11:
            r = xStreamBufferReset( buffer[ b ] );
            snprintf( op, n, "breset %d", b );
            return r;

        default:
            isr_enter();
            r = xStreamBufferSendCompletedFromISR( buffer[ b ], &woken );
            isr_exit( woken );
            snprintf( op, n, "bdone_isr %d", b );
            return r;
    }
}

/* ------------------------------------------------- authored sweeps -- */

/* A sweep (plan P3) is a script written by hand for an arm the random one
 * cannot reach. Each row is the four numbers the xorshift would otherwise
 * draw for a step -- core, kind, slot, arg -- repeated `n` times. A step the
 * loop turns into a daemon run or a coroutine's continuation hands its row
 * back, so a row of 600 ticks is 600 ticks whatever else ran.
 *
 * SETWAKE is the sweeps' own kind: it sets the current task's
 * `pxPreviousWakeTime`, which is driver state and not a kernel call.
 * NOTASET is a call the random script never makes: xQueueAddToSet with a
 * semaphore where the set belongs. */
#define SETWAKE    200u
#define NOTASET    201u

struct row
{
    unsigned core;
    unsigned kind;
    unsigned slot;
    uint32_t arg;
    unsigned n;
};

/* xTaskDelayUntil across the tick wrap, which the random script can never
 * reach: it creates every task with a previous wake of 0, so the clock is
 * never behind it. 600 ticks take the clock from configINITIAL_TICK_COUNT to
 * 0. A previous wake of 0xFFFFFFF0 then takes the overflow branch's false arm
 * (the wake time 0xFFFFFFF5 did not wrap, so it has passed); 0xFFFFFFFE its
 * true arm (the wake time 3 wrapped too and is still ahead: the task
 * delays). Kind 82 with arg 0x4700 is sched_op's dlyuntil, increment 5. */
static const struct row sweep_overflow[] =
{
    { 0, 91,      0, 0,           600 },
    { 0, SETWAKE, 0, 0xFFFFFFF0u, 1   },
    { 0, 82,      0, 0x4700u,     1   },
    { 0, SETWAKE, 0, 0xFFFFFFFEu, 1   },
    { 0, 82,      0, 0x4700u,     1   },
    { 0, 91,      0, 0,           4   },
    { 0, 82,      0, 0x4700u,     2   },
};

/* xQueueAddToSet given something that is not a set. V11.3.1 checks the item
 * size (a set's is a pointer's) and answers pdFAIL; Kairos checks the kind. */
static const struct row sweep_notaset[] =
{
    { 0, NOTASET, 0, 0, 1 },
};

static const struct row * sweep;
static unsigned sweep_rows;
static unsigned sweep_pos;

/* The sweep's next step, or 0 when it is done. */
static int sweep_next( unsigned * core,
                       unsigned * kind,
                       unsigned * slot,
                       uint32_t * arg )
{
    unsigned pos = sweep_pos;

    for( unsigned i = 0; i < sweep_rows; i++ )
    {
        if( pos < sweep[ i ].n )
        {
            *core = sweep[ i ].core % configNUMBER_OF_CORES;
            *kind = sweep[ i ].kind;
            *slot = sweep[ i ].slot;
            *arg = sweep[ i ].arg;
            sweep_pos++;
            return 1;
        }

        pos -= sweep[ i ].n;
    }

    return 0;
}

static unsigned sweep_steps( void )
{
    unsigned n = 0;

    for( unsigned i = 0; i < sweep_rows; i++ )
    {
        n += sweep[ i ].n;
    }

    return n;
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

    if( argc > 3 )
    {
        if( strcmp( argv[ 3 ], "overflow" ) == 0 )
        {
            sweep = sweep_overflow;
            sweep_rows = sizeof sweep_overflow / sizeof sweep_overflow[ 0 ];
        }
        else if( strcmp( argv[ 3 ], "notaset" ) == 0 )
        {
            sweep = sweep_notaset;
            sweep_rows = sizeof sweep_notaset / sizeof sweep_notaset[ 0 ];
        }
        else
        {
            fprintf( stderr, "no sweep named %s\n", argv[ 3 ] );
            return 2;
        }

        steps = sweep_steps();
    }

    sem = xSemaphoreCreateBinary();
    mutex = xSemaphoreCreateMutex();
    rmutex = xSemaphoreCreateRecursiveMutex();
    csem = xSemaphoreCreateCounting( 3, 1 );
    qset = xQueueCreateSet( SET_LEN );
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
    daemon = xTimerGetTimerDaemonTaskHandle();
    daemon_code = fake_last_code;
    daemon_param = fake_last_param;
    configASSERT( daemon_code != body );
    fake_yields = 0;

    for( int c = 0; c < configNUMBER_OF_CORES; c++ )
    {
        switch_core( c );
    }

    /* The setup above is the same on both sides and is not a step; the
     * header says what it chose, and line 0 where it left the kernel. */
    printf( "seed=0x%08lx cores=%d steps=%u tick0=0x%08lx init=%lu,%lu,%lu,%lu%s%s\n",
            ( unsigned long ) seed, configNUMBER_OF_CORES, steps, ( unsigned long ) configINITIAL_TICK_COUNT,
            ( unsigned long ) init[ 0 ], ( unsigned long ) init[ 1 ],
            ( unsigned long ) init[ 2 ], ( unsigned long ) init[ 3 ],
            sweep ? " sweep=" : "", sweep ? argv[ 3 ] : "" );
    line( 0, 0, "start", 0, 0 );

    for( unsigned step = 1; sweep ? 1 : ( step <= steps ); step++ )
    {
        int core;
        unsigned kind;
        unsigned slot;
        unsigned arg;

        if( sweep )
        {
            unsigned c;
            uint32_t a;

            if( !sweep_next( &c, &kind, &slot, &a ) )
            {
                break;
            }

            core = ( int ) c;
            arg = a;
        }
        else
        {
            core = ( int ) ( next() % configNUMBER_OF_CORES );
            kind = next() % 100;
            slot = next() % SLOTS;
            arg = next();
        }

        long r = 0;
        TaskHandle_t t = app[ slot ];

        fake_core = core;
        fake_yields = 0;

        /* A task with a call suspended inside the kernel runs nothing else
         * until that call returns: if it is current here, the step is its
         * continuation. */
        int cur_slot = slot_of( xTaskGetCurrentTaskHandleForCore( core ) );

        /* The daemon runs whenever it is current: until it blocks again. */
        if( xTaskGetCurrentTaskHandleForCore( core ) == daemon )
        {
            r = run_daemon();
            line( step, core, "daemon", r, fake_yields );
            sweep_pos -= ( sweep != NULL );
            continue;
        }

        if( ( cur_slot >= 0 ) && pending[ cur_slot ] )
        {
            r = run_coro( cur_slot );
            line( step, core, "cont", r, fake_yields );
            sweep_pos -= ( sweep != NULL );
            continue;
        }

        if( kind == SETWAKE )
        {
            if( cur_slot >= 0 )
            {
                wake[ cur_slot ] = ( TickType_t ) arg;
            }

            snprintf( op, sizeof op, "setwake %lu", ( unsigned long ) arg );
        }
        else if( kind == NOTASET )
        {
            r = xQueueAddToSet( csem, sem );
            snprintf( op, sizeof op, "saddnotaset" );
        }
        else if( kind < 5 )
        {
            if( t == NULL )
            {
                char n[ 8 ];
                snprintf( n, sizeof n, "t%u", created++ );
                UBaseType_t p = arg % 4;
                r = xTaskCreate( body, n, configMINIMAL_STACK_SIZE, NULL, p, &app[ slot ] );
                wake[ slot ] = 0;
                snprintf( op, sizeof op, "create %u %s %lu", slot, n, ( unsigned long ) p );
            }
            else
            {
                snprintf( op, sizeof op, "noop" );
            }
        }
        else if( kind < 9 )
        {
            /* Not a mutex holder, and not a task blocked on a stream
             * buffer: a buffer records its waiting task by HANDLE, not on a
             * list, so deleting that task leaves the buffer pointing at a
             * freed TCB (the first script that tried it segfaulted). */
            int on_buffer = pending[ slot ] &&
                            ( ( coro_call[ slot ].kind == CALL_BSEND ) || ( coro_call[ slot ].kind == CALL_BRECV ) );

            if( ( t != NULL ) && !holds_a_mutex( t ) && !on_buffer )
            {
                for( int c = 0; c < configNUMBER_OF_CORES; c++ )
                {
                    if( xTaskGetCurrentTaskHandleForCore( c ) == t )
                    {
                        dead = t;
                    }
                }

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
        else if( kind < 14 )
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
        else if( kind < 19 )
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
        else if( kind < 24 )
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
        else if( kind < 28 )
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
        else if( kind < 32 )
        {
            r = xSemaphoreGive( sem );
            snprintf( op, sizeof op, "give" );
        }
        else if( kind < 35 )
        {
            BaseType_t woken = pdFALSE;
            isr_enter();
            r = xSemaphoreGiveFromISR( sem, &woken );
            isr_exit( woken );
            snprintf( op, sizeof op, "give_isr" );
        }
        else if( kind < 39 )
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
        else if( kind < 51 )
        {
            r = queue_op( cur_slot, slot, arg, op, sizeof op );
        }
        else if( kind < 58 )
        {
            r = mutex_op( cur_slot, arg, op, sizeof op );
        }
        else if( kind < 64 )
        {
            r = notify_op( cur_slot, slot, arg, op, sizeof op );
        }
        else if( kind < 69 )
        {
            r = group_op( cur_slot, slot, arg, op, sizeof op );
        }
        else if( kind < 75 )
        {
            r = buffer_op( cur_slot, slot, arg, op, sizeof op );
        }
        else if( kind < 82 )
        {
            r = timer_op( cur_slot, slot, arg, op, sizeof op );
        }
        else if( kind < 87 )
        {
            r = sched_op( cur_slot, slot, arg, op, sizeof op );
        }
        else if( kind < 91 )
        {
            r = set_op( cur_slot, slot, arg, op, sizeof op );
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
