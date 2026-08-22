/*
Studio/Hub -- SuperCollider boot file for the "orbits" stack.

All ports are moved out of the SuperCollider/SuperDirt defaults so that a stray
sclang (an IDE session, another SuperDirt) can never silently take a socket we
depend on:

    6110  scsynth            <- must equal Tidal's oBusPort
    6120  sclang langPort    <- set with `sclang -u 6120`, NOT settable here
    6122  SuperDirt          <- must equal Tidal's oPort

Note there is no Server.killAll and no s.reboot here. Server.default is a fresh
Server object that has never booted, so waitForBoot boots it with the options
below actually applied -- options set inside a reboot callback only take effect
on the *following* boot, which is what made this file's values look inert.
*/
(
// Pin and prioritise scsynth HERE rather than on sclang's own command line.
// A child inherits CPU affinity and scheduling policy, so pinning sclang put
// sclang and scsynth on one core at SCHED_FIFO 80 -- and under FIFO an equal
// priority thread never preempts a running one. While sclang fired ~2500
// /b_allocRead messages during sample loading, scsynth could not be scheduled
// to drain its UDP socket, so the kernel silently discarded them, along with
// initTree's /g_new. Result: sclang believed the buffers and node tree existed
// while the server had neither, with no error on either side.
Server.program = "exec taskset -c 4 chrt -f 80 pw-jack -p 128 scsynth";

// The stock `s` / Server.default is hardwired to 127.0.0.1:57110 at class init,
// so replace the object rather than trying to mutate its address.
Server.default = Server(\orbits, NetAddr("127.0.0.1", 6110));
s = Server.default;

s.options.device = "orbits";        // JACK client name -> orbits:out_1 .. out_36
s.options.sampleRate = 48000;
s.options.hardwareBufferSize = 128;
s.options.blockSize = 128;
s.options.numBuffers = 2048 * 256;  // sample buffers; SuperDirt needs thousands
s.options.memSize = 8192 * 64;      // realtime memory, KB
s.options.numWireBufs = 256;        // raise if "exceeded number of interconnect buffers"
s.options.maxNodes = 1024 * 512;    // raise if "too many nodes"
s.options.numOutputBusChannels = 36;
s.options.numInputBusChannels = 0;
s.options.maxLogins = 3;            // hard limit is 32; every client must agree

s.waitForBoot {
	~dirt = SuperDirt(2, s);
	~dirt.doNotReadYet = false;     // lazy sample loading off
	~dirt.loadSoundFiles;
	~dirt.loadSoundFiles("/home/endo.202511/Studio/Sampling/tidal-samples/*");

	// 18 stereo orbits on even output busses. This port is what Tidal targets.
	~dirt.start(6122, [ 0, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 30, 32, 34 ]);

	(
		MIDIClient.init;
		~midithroughport0 = MIDIOut.newByName("Midi Through", "Midi Through Port-0");
		~midithroughport0.latency = 0;
		~dirt.soundLibrary.addMIDI(\midi0, ~midithroughport0);
	);

	// Convenient orbit handles from sclang.
	(
		~b1 = ~dirt.orbits[0];  ~b2 = ~dirt.orbits[1];  ~b3 = ~dirt.orbits[2];
		~b4 = ~dirt.orbits[3];  ~b5 = ~dirt.orbits[4];  ~b6 = ~dirt.orbits[5];
		~l1 = ~dirt.orbits[6];  ~l2 = ~dirt.orbits[7];  ~l3 = ~dirt.orbits[8];
		~l4 = ~dirt.orbits[9];  ~l5 = ~dirt.orbits[10]; ~l6 = ~dirt.orbits[11];
		~a1 = ~dirt.orbits[12]; ~a2 = ~dirt.orbits[13]; ~a3 = ~dirt.orbits[14];
		~a4 = ~dirt.orbits[15]; ~a5 = ~dirt.orbits[16]; ~a6 = ~dirt.orbits[17];
	);

	// SuperDirt's /n_end responder binds to langPort; if that is not 6120 the
	// flotsam dictionary never gets cleared and /n_set errors pile up forever.
	"HUB: scsynth=% langPort=% dirt=6122 clientID=%".format(
		s.addr.port, NetAddr.langPort, s.clientID
	).postln;
};

s.latency = 0.65;   // raise if you get "late" messages
)
