# EGT80 - "Ethernet Gateway Terminal" for CP/M / Z80
#
# EGT80.COM is built by running the REAL period assembler - SLR Z80ASM -
# under zxcc, John Elliott's CP/M-on-Unix runner (the same way RomWBW
# builds itself).  That is deliberate: if the build passes here, the
# source assembles on a real CP/M machine with the same assembler.  A
# modern cross-assembler would happily accept syntax SLR80 rejects.
#
#   make             assemble both EGT80.COM and EGT8080.COM
#   make port        re-derive EGT8080.Z80 from EGT80.Z80 (do this after
#                    editing EGT80.Z80 - EGT8080.Z80 is a GENERATED file)
#   make check       prove EGT80 also assembles with M80 and with ZMAC
#   make check-m80   Microsoft M80 + L80 only
#   make check-zmac  ZMAC only
#   make tools       build zxcc (once; needs a C compiler)
#   make clean       remove build products (keeps zxcc and EGT80.COM)
#   make distclean   also remove zxcc and EGT80.COM
#
# Line endings: the repo copy of the source is Unix LF so edits and diffs
# stay sane, and every recipe converts it to CRLF when staging into
# build/.  This is not cosmetic - M80 and ZMAC read an LF-only file as one
# enormous line and silently assemble almost nothing.

# --- Where the CP/M tools live -----------------------------------------
# CPMBIN holds the period .COM tools.  RomWBW ships all of them; override
# on the command line if yours are elsewhere:
#     make CPMBIN=/path/to/bin80
CPMBIN  ?= $(HOME)/RomWBW/Tools/cpm/bin80

# zxcc's source, copied in on first build.  RomWBW carries it; it is not
# vendored here (it is GPL software with its own upstream, and tools/ is
# not part of this project's source).
ZXCCSRC ?= $(HOME)/RomWBW/Tools/unix/zxcc

ZXCC    := tools/zxcc-src/zxcc
BUILD   := build
SRC     := EGT80.Z80
TARGET  := EGT80.COM

# The 8080 build.  EGT8080.Z80 is DERIVED from EGT80.Z80 - see
# tools/port8080.py for why, and for every difference between them.  It is
# committed like the .COM files are, because the toolchain that produces it
# is not something CI has.
SRC80   := EGT8080.Z80
TARGET80 := EGT8080.COM

# zxcc maps CP/M drive A: to the working directory, so the source and the
# assembler are staged into $(BUILD) and run there.  Files a CP/M program
# creates arrive in lower case, hence build/egt80.com.
.PHONY: all
all: $(TARGET) $(TARGET80)

# Re-derive the 8080 source.  Deliberately NOT a prerequisite of
# $(TARGET80): regenerating on every build would silently throw away a
# hand-edit of the derived file, and the person who made that edit needs
# to be told to move it into tools/port8080.py, not to lose it.
.PHONY: port
port:
	python3 tools/port8080.py $(SRC) $(SRC80)
	python3 tools/check8080.py $(SRC80)

$(TARGET): $(SRC) $(ZXCC)
	@mkdir -p $(BUILD)
	sed 's/$$/\r/' $(SRC) > $(BUILD)/EGT80.Z80
	cp $(CPMBIN)/Z80ASM.COM $(BUILD)/
	cd $(BUILD) && ../$(ZXCC) ./Z80ASM.COM -EGT80/A | tee slr.log
	@grep -q ' 0 Error(s) Detected' $(BUILD)/slr.log \
	  || (echo "*** SLR Z80ASM reported errors - see $(BUILD)/slr.log"; exit 1)
	@test -s $(BUILD)/egt80.com \
	  || (echo "*** assembly produced no COM file"; exit 1)
	cp $(BUILD)/egt80.com $(TARGET)
	@echo "--- $(TARGET) built: `wc -c < $(TARGET)` bytes"

# The 8080 build.  Same assembler, same rules - what makes it an 8080
# program is the subset of instructions it uses, which check8080.py holds
# rather than the assembler.  That check runs FIRST: a Z80 instruction in
# here assembles perfectly and then crashes an Altair in whatever menu
# nobody exercised, so it must never reach the COM file.
$(TARGET80): $(SRC80) tools/check8080.py $(ZXCC)
	python3 tools/check8080.py $(SRC80)
	@mkdir -p $(BUILD)
	sed 's/$$/\r/' $(SRC80) > $(BUILD)/EGT8080.Z80
	cp $(CPMBIN)/Z80ASM.COM $(BUILD)/
	cd $(BUILD) && ../$(ZXCC) ./Z80ASM.COM -EGT8080/A | tee slr80.log
	@grep -q ' 0 Error(s) Detected' $(BUILD)/slr80.log \
	  || (echo "*** SLR Z80ASM reported errors - see $(BUILD)/slr80.log"; exit 1)
	@test -s $(BUILD)/egt8080.com \
	  || (echo "*** assembly produced no COM file"; exit 1)
	cp $(BUILD)/egt8080.com $(TARGET80)
	@echo "--- $(TARGET80) built: `wc -c < $(TARGET80)` bytes"

# --- Portability gates -------------------------------------------------
# Second and third opinions, so syntax only SLR accepts cannot slip in.
# Each gate must *produce output* to pass: these assemblers report a bad
# read as a warning and exit successfully, so exit status alone proves
# nothing (an early version of this Makefile printed OK while M80 was
# assembling one 6 KB comment line).
#
# M80 needs a leading .Z80 directive - it defaults to 8080 mnemonics.
# That is M80's requirement rather than a property of our source, so it is
# prepended here instead of living in the file (SLR and ZMAC are Z80
# assemblers and need no such line).  ZMAC, like SLR, wants type .Z80.
.PHONY: check check-m80 check-zmac check-m80-80 check-zmac-80
check: check-m80 check-zmac check-m80-80 check-zmac-80

check-m80: $(ZXCC)
	@mkdir -p $(BUILD)
	@printf '\t.Z80\r\n' > $(BUILD)/m80pre.tmp
	sed 's/$$/\r/' $(SRC) | cat $(BUILD)/m80pre.tmp - > $(BUILD)/EGT80M.MAC
	cp $(CPMBIN)/M80.COM $(CPMBIN)/L80.COM $(BUILD)/
	@rm -f $(BUILD)/egt80m.rel $(BUILD)/egt80m.com
	cd $(BUILD) && ../$(ZXCC) ./M80.COM -=EGT80M | tee m80.log
	@grep -q 'No Fatal error(s)' $(BUILD)/m80.log \
	  || (echo "*** M80 reported errors - see $(BUILD)/m80.log"; exit 1)
	@test -s $(BUILD)/egt80m.rel \
	  || (echo "*** M80 produced no REL - it did not assemble the source"; exit 1)
	cd $(BUILD) && ../$(ZXCC) ./L80.COM -EGT80M,EGT80M/N/E
	@test -s $(BUILD)/egt80m.com \
	  || (echo "*** L80 produced no COM"; exit 1)
	@echo "--- M80 + L80: OK (`wc -c < $(BUILD)/egt80m.rel` byte REL)"

check-zmac: $(ZXCC)
	@mkdir -p $(BUILD)
	sed 's/$$/\r/' $(SRC) > $(BUILD)/EGT80Z.Z80
	cp $(CPMBIN)/ZMAC.COM $(BUILD)/
	@rm -f $(BUILD)/egt80z.rel
	-cd $(BUILD) && ../$(ZXCC) ./ZMAC.COM -EGT80Z > zmac.log 2>&1
	@test -s $(BUILD)/egt80z.rel \
	  || (echo "*** ZMAC produced no REL - see $(BUILD)/zmac.log"; exit 1)
	@grep -qi 'assembled with[ ]*NO ERRORS' $(BUILD)/zmac.log \
	  || (echo "*** ZMAC did not report a clean assembly - see $(BUILD)/zmac.log"; exit 1)
	@echo "--- ZMAC: OK (`wc -c < $(BUILD)/egt80z.rel` byte REL)"

# The same two gates for the 8080 source.  Not redundant just because it is
# derived: the port introduces syntax EGT80.Z80 never had - the patched
# operands `LD (IOP1+1),A` and `IN A,(0)`, and `EX DE,HL` - and syntax only
# SLR accepts is precisely what these gates exist to catch.
check-m80-80: $(ZXCC)
	@mkdir -p $(BUILD)
	@printf '\t.Z80\r\n' > $(BUILD)/m80pre.tmp
	sed 's/$$/\r/' $(SRC80) | cat $(BUILD)/m80pre.tmp - > $(BUILD)/EGT88M.MAC
	cp $(CPMBIN)/M80.COM $(CPMBIN)/L80.COM $(BUILD)/
	@rm -f $(BUILD)/egt88m.rel $(BUILD)/egt88m.com
	cd $(BUILD) && ../$(ZXCC) ./M80.COM -=EGT88M | tee m80-80.log
	@grep -q 'No Fatal error(s)' $(BUILD)/m80-80.log \
	  || (echo "*** M80 reported errors - see $(BUILD)/m80-80.log"; exit 1)
	@test -s $(BUILD)/egt88m.rel \
	  || (echo "*** M80 produced no REL - it did not assemble the source"; exit 1)
	cd $(BUILD) && ../$(ZXCC) ./L80.COM -EGT88M,EGT88M/N/E
	@test -s $(BUILD)/egt88m.com \
	  || (echo "*** L80 produced no COM"; exit 1)
	@echo "--- EGT8080 M80 + L80: OK (`wc -c < $(BUILD)/egt88m.rel` byte REL)"

check-zmac-80: $(ZXCC)
	@mkdir -p $(BUILD)
	sed 's/$$/\r/' $(SRC80) > $(BUILD)/EGT88Z.Z80
	cp $(CPMBIN)/ZMAC.COM $(BUILD)/
	@rm -f $(BUILD)/egt88z.rel
	-cd $(BUILD) && ../$(ZXCC) ./ZMAC.COM -EGT88Z > zmac-80.log 2>&1
	@test -s $(BUILD)/egt88z.rel \
	  || (echo "*** ZMAC produced no REL - see $(BUILD)/zmac-80.log"; exit 1)
	@grep -qi 'assembled with[ ]*NO ERRORS' $(BUILD)/zmac-80.log \
	  || (echo "*** ZMAC did not report a clean assembly - see $(BUILD)/zmac-80.log"; exit 1)
	@echo "--- EGT8080 ZMAC: OK (`wc -c < $(BUILD)/egt88z.rel` byte REL)"

# ZSM (also in bin80) is not used as a gate: its command-line syntax
# rejects every form tried under zxcc, and guessing at it proves nothing
# about our source.  SLR + M80 + ZMAC is already three independent
# assemblers.

# --- zxcc --------------------------------------------------------------
.PHONY: tools
tools: $(ZXCC)

$(ZXCC):
	@test -d $(ZXCCSRC) || (echo "*** zxcc source not found at $(ZXCCSRC)"; \
	  echo "*** set ZXCCSRC=/path/to/RomWBW/Tools/unix/zxcc"; exit 1)
	@mkdir -p tools
	@test -d tools/zxcc-src || cp -r $(ZXCCSRC) tools/zxcc-src
	$(MAKE) -C tools/zxcc-src zxcc

.PHONY: clean distclean
clean:
	rm -rf $(BUILD)

distclean: clean
	$(MAKE) -C tools/zxcc-src clean || true
	rm -f $(TARGET) $(TARGET80)
