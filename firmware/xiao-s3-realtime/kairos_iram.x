/* The default (opt out with --features flash-code): run the firmware's, the kernel's and the port's code from
 * IRAM instead of from flash behind the instruction cache.
 *
 * The decomposition found the latency TAILS in the kernel segments while the
 * trap segments never moved. The hypothesis under test: instruction-cache
 * misses on kernel code fetched from flash.
 *
 * What is moved: every function whose mangled name carries `rusty_rtos_`
 * (kernel-core, core, port-xtensa) or `xiao_s3_realtime` (this firmware,
 * including its interrupt handlers and the tasks the kernel is inlined into),
 * and the three esp-hal functions on the interrupt path that esp-hal itself
 * leaves in flash (listed at the block).
 * Each function's `.literal.<f>` stays next to its `.text.<f>` in one input
 * description, as esp-hal's own `text.x` requires for L32R reach.
 *
 * THE TRAP THIS SCRIPT EXISTS TO AVOID: on the S3, D/IRAM is ONE physical
 * SRAM seen at two addresses (0x4037_8000 in the instruction map is
 * 0x3FC8_8000 in the data map, an offset of 0x6F_0000). esp-hal reserves
 * room in DRAM for exactly `.vectors + .rwtext + .rwtext.wifi`, so code added
 * to IRAM by any other section would have `.data` laid over it. Hence the
 * matching reservation below, and the ASSERT, which fails the LINK rather
 * than letting the overlap reach a board.
 */

/* HOW THIS IS PLACED. ld assigns an input section to the FIRST output
 * statement whose pattern matches it, so the block below must precede
 * esp-hal's `.text` catch-all. It must also follow every INSERT: an INSERT
 * moves every statement since the previous one along with it. The only slot
 * that is both is inside esp32s3.x, between its last INSERT and its shared
 * sections. So this file IS linkall.x, with esp32s3.x inlined from
 * esp-hal =1.2.1 (pinned in Cargo.toml) and one block added at that slot.
 * build.rs passes it INSTEAD of linkall.x.
 *
 * Two shapes that do not work, tried first:
 *  - a second -T with `INSERT AFTER .rwtext`: ld applies an INSERT script to
 *    its built-in default script ("`.rwtext` not found for insert");
 *  - this block in a -T ahead of linkall.x: the memory regions are not
 *    declared yet ("memory region `iram_seg' not declared").
 */

/* ---- linkall.x ---- */
INCLUDE "memory.x"
INCLUDE "alias.x"
/* ---- esp32s3.x, esp-hal 1.2.1, up to its shared sections ---- */
INCLUDE exception.x

SECTIONS {
  .rotext_dummy (NOLOAD) :
  {
    /* This dummy section represents the .rodata section within ROTEXT.
    * Since the same physical memory is mapped to both DROM and IROM,
    * we need to make sure the .rodata and .text sections don't overlap.
    * We skip the amount of memory taken by .rodata* in .text
    */

    /* Start at the same alignment constraint than .flash.text */

    . = ALIGN(ALIGNOF(.rodata));
    . = ALIGN(ALIGNOF(.rodata.wifi));

    /* Create an empty gap as big as .text section */

    . = . + SIZEOF(.flash.appdesc);
    . = . + SIZEOF(.rodata);
    . = . + SIZEOF(.rodata.wifi);

    /* Prepare the alignment of the section above. Few bytes (0x20) must be
     * added for the mapping header.
     */

    . = ALIGN(0x10000) + 0x20;
    _rotext_reserved_start = .;
  } > ROTEXT
}
INSERT BEFORE .text;

/* Similar to .rotext_dummy this represents .rwtext but in .data */
SECTIONS {
  .rwdata_dummy (NOLOAD) : ALIGN(4)
  {
    . = . + SIZEOF(.rwtext) + SIZEOF(.rwtext.wifi) + SIZEOF(.vectors);
  } > RWDATA
}
INSERT BEFORE .data;

/* ---- added: the moved code, and its reservation on the data side ---- */
SECTIONS {
  .rwtext.kairos : ALIGN(4)
  {
    *(.literal.*rusty_rtos_* .text.*rusty_rtos_* .literal.*xiao_s3_realtime* .text.*xiao_s3_realtime*)
    /* esp-hal's own interrupt path that it leaves in flash: the critical
     * section every `with_kernel` takes, and the pending-source scan and
     * matrix lookup its #[ram] dispatcher calls for a peripheral interrupt. */
    *(.literal._critical_section_1_0_* .text._critical_section_1_0_*)
    *(.literal.*InterruptStatusIterator* .text.*InterruptStatusIterator*)
    *(.literal.*mapped_to_raw* .text.*mapped_to_raw*)
    . = ALIGN(4);
  } > RWTEXT

  /* Allocated first in RWDATA, ahead of esp-hal's own .rwdata_dummy: the
   * two reservations together cover vectors + this + .rwtext + .rwtext.wifi,
   * which is exactly what IRAM holds. */
  .rwdata_dummy_kairos (NOLOAD) : ALIGN(4)
  {
    . = . + SIZEOF(.rwtext.kairos);
  } > RWDATA
}

/* ---- esp32s3.x, the rest ---- */
/* Shared sections - ordering matters */
SECTIONS {
  INCLUDE "rwtext.x"
  INCLUDE "rwdata.x"
}
INCLUDE "rodata.x"
INCLUDE "text.x"
INCLUDE "rtc_fast.x"
INCLUDE "rtc_slow.x"
INCLUDE "stack.x"
INCLUDE "dram2.x"
INCLUDE "dcache_reclaimed.x"
INCLUDE "metadata.x"
INCLUDE "eh_frame.x"
/* End of Shared sections */
/* ---- linkall.x ---- */
INCLUDE "hal-defaults.x"

ASSERT(ADDR(.data) >= ADDR(.rwtext.kairos) + SIZEOF(.rwtext.kairos) - 0x6F0000,
       "kairos_iram.x: .data overlaps the code moved to IRAM (same physical SRAM)");
ASSERT(ADDR(.data) >= ADDR(.rwtext.wifi) + SIZEOF(.rwtext.wifi) - 0x6F0000,
       "kairos_iram.x: .data overlaps esp-hal's own IRAM code");
ASSERT(SIZEOF(.rwtext.kairos) > 0,
       "kairos_iram.x: nothing matched -- the symbol patterns no longer fit the mangled names");
