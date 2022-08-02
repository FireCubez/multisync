class SyncStreamer extends AudioWorkletProcessor {
	constructor(options) {
		super();
		this.backingData = options.processorOptions;
	}

	process(inputs, outputs, parameters) {
		let resampled = inputs[0][0];
		let output = outputs[0][0];
		if(this.start == null) this.start = currentFrame;
		let idx = currentFrame - this.start;
		if(this.backingData != null && idx + output.length > 0) {
			let s = this.backingData.subarray(
				Math.max(idx, 0), idx + output.length
			);
			output.set(s, output.length - s.length);
		}
		this.port.postMessage(resampled, [resampled.buffer]);
		return true;
	}
}

registerProcessor("sync-streamer", SyncStreamer);
