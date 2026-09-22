/* QEMU `virt`, RV32. The machine has no flash: -bios none loads the ELF
   straight into DRAM at 0x8000_0000 and starts at its entry point, so
   text and data name the same region. */
MEMORY
{
  RAM : ORIGIN = 0x80000000, LENGTH = 16M
}

REGION_ALIAS("REGION_TEXT",   RAM);
REGION_ALIAS("REGION_RODATA", RAM);
REGION_ALIAS("REGION_DATA",   RAM);
REGION_ALIAS("REGION_BSS",    RAM);
REGION_ALIAS("REGION_HEAP",   RAM);
REGION_ALIAS("REGION_STACK",  RAM);
