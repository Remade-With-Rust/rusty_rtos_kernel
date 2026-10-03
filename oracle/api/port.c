/* The fake port's functions. See portmacro.h. */
#include <stdio.h>
#include <stdlib.h>

#include "FreeRTOS.h"
#include "task.h"

volatile int fake_core = 0;
volatile unsigned fake_yields = 0;
volatile int fake_in_isr = 0;

#if configNUMBER_OF_CORES > 1
    UBaseType_t uxCriticalNestings[ 2 ] = { 0, 0 };
#else
    static UBaseType_t uxCriticalNesting = 0;

    void vPortEnterCritical( void )
    {
        uxCriticalNesting++;
    }

    void vPortExitCritical( void )
    {
        configASSERT( uxCriticalNesting > 0 );
        uxCriticalNesting--;
    }
#endif

/* Set by every blocking trace hook: the calling task is about to be placed on
 * an event list, so its next yield is a BLOCK, not a preemption. */
volatile int fake_blocking = 0;

/* What a yield of this core does besides recording it (the driver's
 * coroutine switch). */
void ( * fake_yield_hook )( void ) = NULL;

void fake_trace_blocking( void )
{
    fake_blocking = 1;
}

void fake_yield( void )
{
    fake_yields |= ( 1u << fake_core );

    if( fake_yield_hook != NULL )
    {
        fake_yield_hook();
    }
}

StackType_t * pxPortInitialiseStack( StackType_t * pxTopOfStack,
                                     TaskFunction_t pxCode,
                                     void * pvParameters )
{
    ( void ) pxCode;
    ( void ) pvParameters;
    return pxTopOfStack;
}

/* The scheduler "starts" without running anything: the driver drives it. */
BaseType_t xPortStartScheduler( void )
{
    return pdTRUE;
}

void vPortEndScheduler( void )
{
}

void vAssertCalled( const char * file, int line )
{
    printf( "ASSERT %s:%d\n", file, line );
    exit( 2 );
}
