/* Memory layout for the RP2040 with the W25Q32JVUUIQ external QSPI flash
   on the 0xCB-1337 rev5.0 (4 MB / 32 Mbit). */

MEMORY {
    BOOT2 : ORIGIN = 0x10000000, LENGTH = 0x100
    FLASH : ORIGIN = 0x10000100, LENGTH = 4096K - 0x100
    RAM   : ORIGIN = 0x20000000, LENGTH = 264K
}

EXTERN(BOOT2_FIRMWARE)

SECTIONS {
    /* The RP2040 ROM bootloader runs the first 256 bytes of flash as a
       second-stage that configures XIP. embassy-rp ships a default. */
    .boot2 ORIGIN(BOOT2) :
    {
        KEEP(*(.boot2));
    } > BOOT2
} INSERT BEFORE .text;
