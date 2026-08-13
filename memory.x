MEMORY {
    /*
     * RP2350-Plus (Waveshare) ships with 4 MiB (or 16 MiB) of external QSPI
     * flash. 4096K is safe for every variant; bump to 16384K on the 16 MiB board.
     */
    FLASH : ORIGIN = 0x10000000, LENGTH = 4096K
    /*
     * RAM consists of 8 banks, SRAM0-SRAM7, with a striped mapping.
     */
    RAM : ORIGIN = 0x20000000, LENGTH = 512K
    /*
     * RAM banks 8 and 9 use a direct mapping, useful for dedicated buffers
     * (e.g. separate stacks per core).
     */
    SRAM8 : ORIGIN = 0x20080000, LENGTH = 4K
    SRAM9 : ORIGIN = 0x20081000, LENGTH = 4K
}

SECTIONS {
    /* ### Boot ROM info
     *
     * Goes after .vector_table, to keep it in the first 4K of flash
     * where the Boot ROM (and picotool) can find it.
     */
    .start_block : ALIGN(4)
    {
        __start_block_addr = .;
        KEEP(*(.start_block));
        KEEP(*(.boot_info));
    } > FLASH

} INSERT AFTER .vector_table;

/* move .text to start /after/ the boot info */
_stext = ADDR(.start_block) + SIZEOF(.start_block);

SECTIONS {
    /* ### Picotool 'Binary Info' Entries */
    .bi_entries : ALIGN(4)
    {
        __bi_entries_start = .;
        KEEP(*(.bi_entries));
        . = ALIGN(4);
        __bi_entries_end = .;
    } > FLASH
} INSERT AFTER .text;

SECTIONS {
    /* ### Boot ROM extra info
     *
     * Goes after everything in our program, so it can contain a signature.
     */
    .end_block : ALIGN(4)
    {
        __end_block_addr = .;
        KEEP(*(.end_block));
    } > FLASH

} INSERT AFTER .uninit;

PROVIDE(start_to_end = __end_block_addr - __start_block_addr);
PROVIDE(end_to_start = __start_block_addr - __end_block_addr);
