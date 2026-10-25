/* Static, non-PIE ET_EXEC layout for the BTB VM: R+X code, R read-only data and
   one R+W segment (data and zero-filled bss). Entry points are kept by --undefined (see
   .cargo/config.toml). */
ENTRY(btb_query_list_mine)

PHDRS {
  text PT_LOAD FLAGS(5);
  rodata PT_LOAD FLAGS(4);
  data PT_LOAD FLAGS(6);
}

SECTIONS {
  . = 0x10000;
  .text : { *(.text .text.*) } :text
  . = ALIGN(4096);
  .rodata : { *(.rodata .rodata.*) *(.srodata .srodata.*) } :rodata
  . = ALIGN(4096);
  .data : { *(.data .data.*) *(.sdata .sdata.*) } :data
  .bss : ALIGN(16) { *(.sbss .sbss.*) *(.bss .bss.*) *(COMMON) } :data
  /DISCARD/ : { *(.eh_frame*) *(.comment) *(.riscv.attributes) *(.note*) }
}
