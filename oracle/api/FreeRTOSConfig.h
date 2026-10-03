/* The API differential's FreeRTOS configuration: ONE source, built twice by
 * run.sh -- `-DCORES=1` and `-DCORES=2` -- so the one-core and the two-core
 * kernel answer the same script. `tests/api_differential.rs` mirrors this
 * file field for field in its two `Config`s. */
#ifndef FREERTOS_CONFIG_H
#define FREERTOS_CONFIG_H

#ifndef CORES
    #error "build with -DCORES=1 or -DCORES=2"
#endif

#define configNUMBER_OF_CORES                   CORES
#if CORES > 1
    #define configRUN_MULTIPLE_PRIORITIES       1
    #define configUSE_CORE_AFFINITY             0
    #define configUSE_PASSIVE_IDLE_HOOK         0
#endif
#define configUSE_PREEMPTION                    1
#define configUSE_TIME_SLICING                  1
#define configUSE_PORT_OPTIMISED_TASK_SELECTION 0
#define configUSE_IDLE_HOOK                     0
#define configUSE_TICK_HOOK                     0
#define configTICK_RATE_HZ                      1000
#define configMAX_PRIORITIES                    5
#define configMINIMAL_STACK_SIZE                128
#define configMAX_TASK_NAME_LEN                 8
#define configTICK_TYPE_WIDTH_IN_BITS           TICK_TYPE_WIDTH_32_BITS
/* 600 ticks short of the wrap: every script crosses it about halfway, so
 * the delayed lists swap, the timer lists switch, and every timeout and
 * delay-until runs its overflow arms. */
#define configINITIAL_TICK_COUNT                ( ( TickType_t ) 0xFFFFFDA8U )
#define configIDLE_SHOULD_YIELD                 1
#define configUSE_MUTEXES                       1
#define configUSE_RECURSIVE_MUTEXES             1
#define configUSE_COUNTING_SEMAPHORES           1
#define configUSE_QUEUE_SETS                    1
/* Tickless compiled in for vTaskStepTick, which the script calls the way a
 * port's suppressed-tick sleep does: scheduler suspended, one tick. The
 * idle task never runs its body here, so nothing else changes. */
#define configUSE_TICKLESS_IDLE                  1
#define configUSE_TASK_NOTIFICATIONS            1
#define configTASK_NOTIFICATION_ARRAY_ENTRIES   1
#define configUSE_TIMERS                        1
#define configTIMER_TASK_PRIORITY               2
#define configTIMER_QUEUE_LENGTH                1
#define configTIMER_TASK_STACK_DEPTH            128
#define configSUPPORT_DYNAMIC_ALLOCATION        1
#define configSUPPORT_STATIC_ALLOCATION         0
#define configTOTAL_HEAP_SIZE                   ( 1024 * 1024 )
#define configUSE_TRACE_FACILITY                0
#define configCHECK_FOR_STACK_OVERFLOW          0
#define configUSE_MALLOC_FAILED_HOOK            0

#define INCLUDE_vTaskDelete                     1
#define INCLUDE_vTaskSuspend                    1
#define INCLUDE_vTaskPrioritySet                1
#define INCLUDE_uxTaskPriorityGet               1
#define INCLUDE_vTaskDelay                      1
#define INCLUDE_eTaskGetState                   1
#define INCLUDE_xTaskGetCurrentTaskHandle       1
#define INCLUDE_xQueueGetMutexHolder            1
#define INCLUDE_xTimerGetTimerDaemonTaskHandle  1
#define INCLUDE_xTimerPendFunctionCall          1
#define INCLUDE_xTaskGetHandle                  1
#define INCLUDE_xTaskAbortDelay                 1
#define INCLUDE_xTaskDelayUntil                 1

void vAssertCalled( const char * file, int line );
#define configASSERT( x )    if( ( x ) == 0 ) vAssertCalled( __FILE__, __LINE__ )

/* Every way a task can place itself on an event list: the yield that follows
 * is a BLOCK, and the driver's coroutine leaves the kernel there. */
void fake_trace_blocking( void );
#define traceBLOCKING_ON_QUEUE_RECEIVE( q )             fake_trace_blocking()
#define traceBLOCKING_ON_QUEUE_PEEK( q )                fake_trace_blocking()
#define traceBLOCKING_ON_QUEUE_SEND( q )                fake_trace_blocking()
#define traceBLOCKING_ON_STREAM_BUFFER_RECEIVE( s )     fake_trace_blocking()
#define traceBLOCKING_ON_STREAM_BUFFER_SEND( s )        fake_trace_blocking()
#define traceEVENT_GROUP_SYNC_BLOCK( g, s, w )          fake_trace_blocking()
#define traceEVENT_GROUP_WAIT_BITS_BLOCK( g, w )        fake_trace_blocking()
#define traceTASK_NOTIFY_TAKE_BLOCK( i )                fake_trace_blocking()
#define traceTASK_NOTIFY_WAIT_BLOCK( i )                fake_trace_blocking()
/* The timer daemon's block: `vQueueWaitForMessageRestricted` places it with
 * no BLOCKING trace of its own. Only the daemon ever calls this. */
#define traceENTER_vTaskPlaceOnEventListRestricted( l, t, w )    fake_trace_blocking()
/* How deep the scheduler is suspended. A blocking call's trace hook fires
 * BEFORE it places the task, and between the two a critical-section exit can
 * take a yield pended by an ISR with no woken pointer (on two cores,
 * `vTaskExitCritical` yields even while suspended). A real port's switch
 * declines that one and the task carries on, so only a yield with the
 * scheduler running is the block. Every resume decrements before it can
 * yield, so counting at entry is exact. */
extern volatile int fake_suspended;
#define traceENTER_vTaskSuspendAll()    ( fake_suspended++ )
#define traceENTER_xTaskResumeAll()     ( fake_suspended-- )

#endif /* FREERTOS_CONFIG_H */
