/* The fake port's functions. See portmacro.h. */
#include <stdio.h>
#include <stdlib.h>
#include "FreeRTOS.h"
#include "task.h"

volatile int fake_core = 0;
volatile unsigned fake_yields = 0;
volatile int fake_in_isr = 0;
UBaseType_t uxCriticalNestings[ 2 ] = { 0, 0 };

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
