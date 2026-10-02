/* The SMP differential's configuration: two cores, preemption and time
 * slicing on, RUN_MULTIPLE_PRIORITIES = 1 (what SMP ports ship; the C default
 * is 0), no core affinity. Kept in step with `tests/smp_differential.rs`. */
#ifndef FREERTOS_CONFIG_H
#define FREERTOS_CONFIG_H

#define configNUMBER_OF_CORES                   2
#define configRUN_MULTIPLE_PRIORITIES           1
#define configUSE_CORE_AFFINITY                 0
#define configUSE_PASSIVE_IDLE_HOOK             0
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
#define configIDLE_SHOULD_YIELD                 1
#define configUSE_MUTEXES                       1
#define configUSE_COUNTING_SEMAPHORES           1
#define configUSE_TIMERS                        1
#define configTIMER_TASK_PRIORITY               4
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
#define INCLUDE_xTaskGetCurrentTaskHandle       1
#define INCLUDE_xTimerGetTimerDaemonTaskHandle  1

void vAssertCalled( const char * file, int line );

/* The blocking-waits driver needs to know when a take is about to block. */
void fake_trace_blocking( void );
#define traceBLOCKING_ON_QUEUE_RECEIVE( pxQueue )    fake_trace_blocking()
#define configASSERT( x )    if( ( x ) == 0 ) vAssertCalled( __FILE__, __LINE__ )

#endif
