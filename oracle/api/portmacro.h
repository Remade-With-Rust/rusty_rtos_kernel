/* A FAKE port for a single-threaded differential driver, one core or two.
 *
 * Nothing runs concurrently and no task body ever executes on its own: the
 * driver calls the kernel's entry points directly and decides, per step,
 * which core is calling (`fake_core`). Every yield the kernel asks for --
 * this core's (`portYIELD`) or another's (`portYIELD_CORE`) -- is RECORDED in
 * `fake_yields` instead of taken; the driver then runs `vTaskSwitchContext`
 * for each core named. The two-core macro shape follows the pinned RP2040
 * SMP port, as `../smp/portmacro.h` does. */
#ifndef PORTMACRO_H
#define PORTMACRO_H

#include <stdint.h>

#define portCHAR          char
#define portFLOAT         float
#define portDOUBLE        double
#define portLONG          long
#define portSHORT         short
#define portSTACK_TYPE    uint32_t
#define portBASE_TYPE     long

typedef portSTACK_TYPE StackType_t;
typedef long BaseType_t;
typedef unsigned long UBaseType_t;
typedef uint32_t TickType_t;

#define portMAX_DELAY              ( TickType_t ) 0xffffffffUL
#define portTICK_TYPE_IS_ATOMIC    1
#define portSUPPRESS_TICKS_AND_SLEEP( x )
#define portSTACK_GROWTH           ( -1 )
#define portTICK_PERIOD_MS         ( ( TickType_t ) 1000 / configTICK_RATE_HZ )
#define portBYTE_ALIGNMENT         8
#define portPOINTER_SIZE_TYPE      uintptr_t
#define portDONT_DISCARD           __attribute__( ( used ) )

extern volatile int fake_core;
extern volatile unsigned fake_yields;
extern volatile int fake_in_isr;

/* A function, not a store, so a blocking call's coroutine can be hooked: a
 * task that has just placed itself on an event list leaves the kernel HERE,
 * as it would on a real port, and resumes here when it next runs. */
void fake_yield( void );
#define portYIELD()                 fake_yield()
#define portYIELD_FROM_ISR( x )     do { if( x ) { portYIELD(); } } while( 0 )
#define portEND_SWITCHING_ISR( x )  portYIELD_FROM_ISR( x )

#define portSET_INTERRUPT_MASK_FROM_ISR()         ( 0u )
#define portCLEAR_INTERRUPT_MASK_FROM_ISR( x )    ( ( void ) ( x ) )
#define portDISABLE_INTERRUPTS()
#define portENABLE_INTERRUPTS()

#if configNUMBER_OF_CORES > 1
    extern UBaseType_t uxCriticalNestings[ 2 ];
    #define portMAX_CORE_COUNT        2
    #define portGET_CORE_ID()         ( ( BaseType_t ) fake_core )
    #define portYIELD_CORE( a )       ( fake_yields |= ( 1u << ( a ) ) )
    #define portCHECK_IF_IN_ISR()     ( fake_in_isr )
    #define portCRITICAL_NESTING_IN_TCB    0
    #define portGET_CRITICAL_NESTING_COUNT( xCoreID )          ( uxCriticalNestings[ ( xCoreID ) ] )
    #define portSET_CRITICAL_NESTING_COUNT( xCoreID, x )       ( uxCriticalNestings[ ( xCoreID ) ] = ( x ) )
    #define portINCREMENT_CRITICAL_NESTING_COUNT( xCoreID )    ( uxCriticalNestings[ ( xCoreID ) ]++ )
    #define portDECREMENT_CRITICAL_NESTING_COUNT( xCoreID )    ( uxCriticalNestings[ ( xCoreID ) ]-- )
    #define portSET_INTERRUPT_MASK()                  ( 0u )
    #define portCLEAR_INTERRUPT_MASK( x )             ( ( void ) ( x ) )
    void vTaskEnterCritical( void );
    void vTaskExitCritical( void );
    UBaseType_t vTaskEnterCriticalFromISR( void );
    void vTaskExitCriticalFromISR( UBaseType_t uxSavedInterruptStatus );
    #define portENTER_CRITICAL()               vTaskEnterCritical()
    #define portEXIT_CRITICAL()                vTaskExitCritical()
    #define portENTER_CRITICAL_FROM_ISR()      vTaskEnterCriticalFromISR()
    #define portEXIT_CRITICAL_FROM_ISR( x )    vTaskExitCriticalFromISR( x )
    #define portGET_ISR_LOCK( xCoreID )
    #define portRELEASE_ISR_LOCK( xCoreID )
    #define portGET_TASK_LOCK( xCoreID )
    #define portRELEASE_TASK_LOCK( xCoreID )
#else
    /* One core: the port owns the nesting count, as every single-core port
     * does. Nothing is masked because nothing can interrupt. */
    void vPortEnterCritical( void );
    void vPortExitCritical( void );
    #define portENTER_CRITICAL()               vPortEnterCritical()
    #define portEXIT_CRITICAL()                vPortExitCritical()
#endif

#define portTASK_FUNCTION_PROTO( vFunction, pvParameters )    void vFunction( void * pvParameters )
#define portTASK_FUNCTION( vFunction, pvParameters )          void vFunction( void * pvParameters )
#define portNOP()
#define portMEMORY_BARRIER()

#endif /* PORTMACRO_H */
