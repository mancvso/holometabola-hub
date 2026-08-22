/*
Studio/Hub -- multi-track recorder.

s.record would give a single mixdown of the master bus. We run 18 stereo orbits
and want one stem per orbit, so each orbit's output bus gets its own DiskOut
synth writing to its own ring buffer and its own file.

Stems are named after the Tidal groups defined in startup.scd's CONFIG block --
currently b1..b6 (beats), l1..l6 (leads), a1..a6 (ambients). The count and the
names are derived, so 36 orbits across 4 groups needs no change in this file:
it records one stem per entry in ~dirt.orbits, whatever that turns out to be.

Output goes to Hub/recordings/<label>_<timestamp>/<stem>.wav

Reloadable: this file only defines things. Re-loading it while a recording is
running leaves that recording alone.
*/
(
~hubRecDir = "/home/endo/Studio/Hub/recordings";

// Taps an orbit's output bus. Lives at the tail of the default group so it
// runs after SuperDirt's monitor synths have written that bus this cycle.
SynthDef(\hubRecorder, { |bufnum = 0, inBus = 0|
	DiskOut.ar(bufnum, In.ar(inBus, 2));
}).add;

// Stem names come from the CONFIG block in startup.scd, so growing to 36 orbits
// across 4 groups renames and re-counts the stems with no edit here. Falls back
// to a flat orbit<n> naming if this file is loaded standalone.
~hubRecStemName = { |i|
	if(~hubGroups.notNil and: { ~hubPerGroup.notNil }) {
		~hubGroups[i div: ~hubPerGroup] ++ ((i mod: ~hubPerGroup) + 1)
	} {
		"orbit" ++ (i + 1)
	}
};

~hubRecStart = { |label = "session"|
	if(~hubRecSynths.notNil) {
		"HUB REC: already recording -- stop first".postln;
	} {
		if(~dirt.isNil) {
			"HUB REC: SuperDirt not running".postln;
		} {
			fork {
				var dir = ~hubRecDir +/+ (label ++ "_" ++ Date.localtime.stamp);
				File.mkdir(~hubRecDir);
				File.mkdir(dir);

				// Allocate every ring buffer first, then open every file, each
				// stage separated by a sync -- the server must have the buffer
				// before /b_write can attach a file to it.
				~hubRecBufs = ~dirt.orbits.collect { Buffer.alloc(s, 65536, 2) };
				s.sync;

				~hubRecBufs.do { |b, i|
					b.write(
						dir +/+ (~hubRecStemName.value(i) ++ ".wav"),
						"wav", "int24", 0, 0, true    // leaveOpen for DiskOut
					);
				};
				s.sync;

				~hubRecSynths = ~dirt.orbits.collect { |orbit, i|
					Synth.tail(s.defaultGroup, \hubRecorder, [
						bufnum: ~hubRecBufs[i],
						inBus: orbit.outBus
					])
				};
				s.sync;

				~hubRecPath = dir;
				("HUB REC: recording % stems -> %".format(~hubRecSynths.size, dir)).postln;
			}
		}
	}
};

~hubRecStop = {
	if(~hubRecSynths.isNil) {
		"HUB REC: not recording".postln;
	} {
		fork {
			~hubRecSynths.do(_.free);
			~hubRecSynths = nil;
			s.sync;
			0.3.wait;                       // let DiskOut flush its last block
			~hubRecBufs.do { |b| b.close; b.free };
			~hubRecBufs = nil;
			s.sync;
			("HUB REC: stopped -> %".format(~hubRecPath ? "?")).postln;
		}
	}
};

"HUB: recording module loaded".postln;
)
