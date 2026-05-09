/* Memory layout for the RP2040 with the W25Q32JVUUIQ external QSPI flash
   on the 0xCB-1337 rev5.0 (4 MB / 32 Mbit).

   The last 16 KB (4 erase sectors) of flash is reserved for persistent
   settings via `sequential-storage`. The linker enforces that firmware code
   cannot overlap with this region — the offsets `__config_start` /
   `__config_end` are exposed below for the Rust storage module. */

MEMORY {
    BOOT2  : ORIGIN = 0x10000000,                LENGTH = 0x100
    FLASH  : ORIGIN = 0x10000100,                LENGTH = 4096K - 0x100 - 16K
    CONFIG : ORIGIN = 0x10000000 + 4096K - 16K,  LENGTH = 16K
    RAM    : ORIGIN = 0x20000000,                LENGTH = 264K
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

/* Flash-relative offsets (i.e. addresses minus XIP_BASE = ORIGIN(BOOT2)).
   `embassy_rp::flash::Flash` and `sequential-storage` both want offsets,
   not XIP virtual addresses. */
__config_start = ORIGIN(CONFIG) - ORIGIN(BOOT2);
__config_end   = __config_start + LENGTH(CONFIG);
