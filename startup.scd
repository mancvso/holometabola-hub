/*
Studio/Hub -- SuperCollider boot file for the "orbits" stack.

Everything scale-dependent is derived from the CONFIG block below. Growing from
18 stereo orbits in 3 groups to 36 in 4 is a two-line edit there; the output bus
count, the orbit bus list, the ~b1../~l1.. handles and the recorder stem names
all follow from it. Nothing downstream hardcodes 18, 36 or the group letters.

Ports are held out of the SuperCollider/SuperDirt defaults so a stray sclang (an
IDE session, another SuperDirt) can never silently take a socket we depend on:

    6110  scsynth            <- must equal Tidal's oBusPort
    6120  sclang langPort    <- set with `sclang -u 6120`, NOT settable here
    6122  SuperDirt          <- must equal Tidal's oPort

Note there is no Server.killAll and no s.reboot here. Server.default is a fresh
Server object that has never booted, so waitForBoot boots it with the options
below actually applied -- options set inside a reboot callback only take effect
on the *following* boot, which is what made this file's values look inert.
*/
(
// ---------------------------------------------------------------- CONFIG ----
~hubGroups     = [ "b", "l", "a" ];   // one entry per group; add a 4th here
~hubPerGroup   = 6;                   // orbits per group; 9 gives 36 total
~hubScsynthPort = 6110;
~hubDirtPort    = 6122;
~hubSampleRate  = 48000;
~hubCore        = 4;                  // CPU core reserved for scsynth

/*
Remote Tidal (Tidal on the laptop, this stack on the headless box).

sclang and scsynth are always co-located -- sclang boots scsynth as a child --
so sclang always talks to it over loopback. What changes for a remote Tidal is
who else is allowed to reach them, and these are two independent things:

  ~hubScsynthBind  what scsynth binds to. Tidal writes control busses DIRECTLY
                   to scsynth via oBusPort, so on loopback-only a remote Tidal's
                   bus writes vanish with no error. Set "0.0.0.0" to open it.
  ~hubTidalSender  the source filter on SuperDirt's OSC responders. The stock
                   NetAddr("127.0.0.1") drops everything from another host. Set
                   it to the laptop's IP, or nil to accept from anywhere.

Both are the "just an IP change" -- but changing only one looks like a total
silence with a perfectly healthy-looking log on both machines.
*/
~hubScsynthBind = "127.0.0.1";
~hubTidalSender = NetAddr("127.0.0.1");

// Derived -- do not edit.
~hubOrbits   = ~hubGroups.size * ~hubPerGroup;
~hubOutChans = ~hubOrbits * 2;                        // stereo per orbit
~hubBusses   = (0, 2 .. (~hubOrbits - 1) * 2);        // even bus per orbit
// ------------------------------------------------------------ END CONFIG ----

// Pin and prioritise scsynth HERE rather than on sclang's own command line.
// A child inherits CPU affinity and scheduling policy, so pinning sclang put
// sclang and scsynth on one core at SCHED_FIFO 80 -- and under FIFO an equal
// priority thread never preempts a running one. While sclang fired ~2500
// /b_allocRead messages during sample loading, scsynth could not be scheduled
// to drain its UDP socket, so the kernel silently discarded them, along with
// initTree's /g_new. Result: sclang believed the buffers and node tree existed
// while the server had neither, with no error on either side.
Server.program = "exec taskset -c % chrt -f 80 pw-jack -p 512 scsynth -B %"
	.format(~hubCore, ~hubScsynthBind);

// The stock `s` / Server.default is hardwired to 127.0.0.1:57110 at class init,
// so replace the object rather than trying to mutate its address. sclang always
// reaches its own child over loopback regardless of what scsynth binds.
Server.default = Server(\orbits, NetAddr("127.0.0.1", ~hubScsynthPort));
s = Server.default;

s.options.device = "orbits";        // JACK client name -> orbits:out_1 ..
s.options.sampleRate = ~hubSampleRate;
s.options.hardwareBufferSize = 512;
s.options.blockSize = 512;
s.options.numBuffers = 2048 * 256;  // sample buffers; SuperDirt needs thousands
s.options.memSize = 8192 * 64;      // realtime memory, KB
s.options.numWireBufs = 256;        // raise if "exceeded number of interconnect buffers"
s.options.maxNodes = 1024 * 512;    // raise if "too many nodes"
s.options.numOutputBusChannels = ~hubOutChans;
s.options.numInputBusChannels = 0;
s.options.maxLogins = 3;            // hard limit is 32; every client must agree

// Each orbit needs private synth/dry/globalEffect busses on top of the hardware
// outs, so this has to grow with the orbit count rather than stay at a constant.
s.options.numAudioBusChannels = max(1024, ~hubOutChans + (~hubOrbits * 8) + 128);

s.waitForBoot {
	~dirt = SuperDirt(2, s);
	~dirt.doNotReadYet = false;     // lazy sample loading off
	~dirt.loadSoundFiles;
	~dirt.loadSoundFiles("/home/endo.202511/Studio/Sampling/tidal-samples/*");

	// A barrier before start: SuperDirt's own *start does this, and unbarriered
	// bursts are exactly how the sample allocReads went missing before.
	s.sync;

	~dirt.start(~hubDirtPort, ~hubBusses, ~hubTidalSender);

	(
		MIDIClient.init;
		~midithroughport0 = MIDIOut.newByName("Midi Through", "Midi Through Port-0");
		~midithroughport0.latency = 0;
		~dirt.soundLibrary.addMIDI(\midi0, ~midithroughport0);
	);

	// Orbit handles: ~b1..~b<n>, ~l1.., ~a1.. built from CONFIG rather than
	// written out, so a 4th group or a different group size needs no edit here.
	~hubGroups.do { |g, gi|
		~hubPerGroup.do { |i|
			currentEnvironment.put(
				(g ++ (i + 1)).asSymbol,
				~dirt.orbits[(gi * ~hubPerGroup) + i]
			);
		};
	};

	// Reloadable modules. Each only defines things, so any of them can be
	// re-.load'ed from the app at runtime without rebooting the stack.
	"/home/endo/Studio/Hub/recording.scd".load;

	// SuperDirt's /n_end responder binds to langPort; if that is not 6120 the
	// flotsam dictionary never gets cleared and /n_set errors pile up forever.
	"HUB: scsynth=% langPort=% dirt=% clientID=% orbits=% outChans=%".format(
		s.addr.port, NetAddr.langPort, ~hubDirtPort, s.clientID,
		~hubOrbits, ~hubOutChans
	).postln;
};

s.latency = 0.65;   // raise if you get "late" messages
)
