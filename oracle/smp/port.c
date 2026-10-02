/* The fake port's functions. See portmacro.h. */
#include <stdio.h>
#include <stdlib.h>
#include "FreeRTOS.h"
#include "task.h"

volatile int fake_core = 0;
volatile unsigned fake_yields = 0;
volatile int fake_in_isr = 0;
UBaseType_t uxCriticalNestings[ 2 ] = { 0, 0 };

/* Set by `traceBLOCKING_ON_QUEUE_RECEIVE`: the calling task is about to be
 * placed on an event list, so its next yield is a BLOCK, not a preemption. */
volatile int fake_blocking = 0;
/* What a yield of this core does besides recording it; NULL unless the
 * driver runs blocking waits. */
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

StackType_t * pxPortInitialiseStack( StackType_t * pxTopOfStack, TaskFunction_t pxCode, void * pvParameters )
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
