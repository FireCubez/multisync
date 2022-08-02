function* gen(wait, count, self) {
	let i = 0;
	let total = 0;
	let beat = 0;
	let [buf, sample] = yield;
	while(beat !== self.stopSyncBeat) {
		console.log(beat, self.stopSyncBeat);
		if(beat < self.stopSyncBeat) {
			self.port.postMessage(self.stopSyncBeat - beat);
		}
		for(let j = 0; j < 2400; j++) {
			if(i === buf.length) {
				[buf, sample] = yield;
				i = 0;
			}
			if(beat % 4 === 0) {
				buf[i] = j % 24 >= 12 ? self.vol : -self.vol;
			} else {
				buf[i] = j % 48 >= 24 ? self.vol : -self.vol;
			}
			i++;
			total++;
		}
		for(let j = 2400; j < 24000;) {
			if(i === buf.length) {
				[buf, sample] = yield;
				i = 0;
			}
			if(j + buf.length - i <= 24000) {
				j += buf.length - i;
				total += buf.length - i;
				i = buf.length;
			} else {
				j++;
				i++;
				total++;
			}
		}
		beat++;
	}
	for(let k = 0; k < count; k++) {
		self.port.postMessage(-k);
		for(let j = 0; j < wait;) {
			if(i === buf.length) {
				[buf, sample] = yield;
				i = 0;
			}
			if(j + buf.length - i <= wait) {
				j += buf.length - i;
				total += buf.length - i;
				i = buf.length;
			} else {
				j++;
				i++;
				total++;
			}
		}
	}
	self.port.postMessage(-count);
	if(self.backingData == null) return;
	for(let j = 0; j < self.backingData.length;) {
		if(i === buf.length) {
			[buf, sample] = yield;
			i = 0;
		}
		if(j + buf.length - i <= self.backingData.length) {
			buf.set(self.backingData.subarray(j, j + buf.length - i), i);
			j += buf.length - i;
			total += buf.length - i;
			i = buf.length;
		} else {
			buf[j] = self.backingData[j];
			j++;
			i++;
			total++;
		}
	}
}

class ServerStreamer extends AudioWorkletProcessor {
	constructor(options) {
		super();
		this.backingData = options.processorOptions.backingData;
		this.syncing = true;
		this.syncBeat = -1;
		this.syncTotalBeats = 0;
		this.stopSyncBeat = -1;
		this.vol = options.processorOptions.vol;
		this.port.onmessage = e => {
			if(e.data.syncBeat) {
				this.stopSyncBeat = e.data.val;
			} else {
				this.vol = e.data.val;
			}
		}
		this.generator = gen(options.processorOptions.backingWait, options.processorOptions.backingWaitCount, this);
		this.generator.next();
	}

	process(inputs, outputs, parameters) {
		let input = inputs[0][0];
		let output = outputs[0][0];
		
		if(this.start == null) { this.start = currentFrame; }
		let time = currentFrame - this.start;
		
		this.port.postMessage("tim:" + time);
		this.port.postMessage(input, [input.buffer]);

		this.generator.next([output, time]);
		return true;
	}
}

registerProcessor("server-streamer", ServerStreamer);
