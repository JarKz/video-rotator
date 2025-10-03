#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use ffmpeg_next as ffmpeg;
use slint::{ComponentHandle, Model, ModelRc, ToSharedString, VecModel};

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::Duration,
};

slint::include_modules!();

const SUPPORTED_EXTENSIONS: [&str; 2] = ["mp4", "mkv"];

fn main() -> anyhow::Result<()> {
    ffmpeg::init()?;

    let window = MainWindow::new()?;
    let thread_pool: Arc<Mutex<Vec<JoinHandle<anyhow::Result<()>>>>> = Arc::new(Mutex::new(vec![]));

    let empty_file_infos: VecModel<FileInfo> = VecModel::from(vec![]);
    let model = ModelRc::new(empty_file_infos);
    window.set_file_infos(model);

    window.on_pick_directory(|| {
        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
            DirectoryInfo {
                path: folder.to_string_lossy().to_shared_string(),
            }
        } else {
            DirectoryInfo {
                path: "".to_shared_string(),
            }
        }
    });

    let weak_window = window.as_weak();

    window.on_pick_files(move || {
        let Some(files) = rfd::FileDialog::new().pick_files() else {
            return;
        };

        for file in files {
            if !file.is_file() {
                return;
            }

            let Some(file_extension) = file.extension() else {
                return;
            };

            if !SUPPORTED_EXTENSIONS.contains(&&*file_extension.to_string_lossy()) {
                return;
            }

            let file_info = FileInfo {
                name: file
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_shared_string(),
                path: file.to_string_lossy().to_shared_string(),
            };

            weak_window
                .clone()
                .upgrade_in_event_loop(|window| {
                    let file_infos_model = window.get_file_infos();
                    let file_infos = file_infos_model
                        .as_any()
                        .downcast_ref::<VecModel<FileInfo>>()
                        .unwrap();

                    if !file_infos.iter().any(|fi| fi == file_info) {
                        file_infos.push(file_info);
                    }
                })
                .unwrap();
        }
    });

    let weak_window = window.as_weak();
    let referenced_thread_pool = thread_pool.clone();
    window.on_rotate_videos(move || {
        let window = weak_window.upgrade().unwrap();
        let file_infos_model = window.get_file_infos();
        let file_infos = file_infos_model
            .as_any()
            .downcast_ref::<VecModel<FileInfo>>()
            .unwrap();
        let output_directory = window.get_output_directory();
        let output_path = Path::new(output_directory.path.as_str());
        let rotation_value = window.get_rotation_value();

        for file_info in file_infos.iter() {
            let file_path = Path::new(file_info.path.as_str());
            let file_name = file_path.file_stem().unwrap();
            let file_extension = file_path.extension().unwrap();

            let mut count = 1;
            let mut output_file_path = {
                let mut output = output_path.to_owned();
                output.push(file_name);
                output = output.with_extension(file_extension);
                output
            };

            while output_file_path.exists() {
                output_file_path = output_path.to_owned();
                let new_file_name =
                    file_name.to_string_lossy().into_owned() + "(" + &count.to_string() + ")";
                output_file_path.push(new_file_name);
                output_file_path = output_file_path.with_extension(file_extension);

                count += 1;
            }

            let mut guard = referenced_thread_pool.lock().unwrap();
            guard.push(std::thread::spawn(move || {
                let mut pipeline =
                    Pipeline::init(file_info.path, output_file_path, rotation_value.into())?;
                pipeline.write_header()?;
                pipeline.configure()?;
                pipeline.pump_packets()?;
                pipeline.write_trailer()?;

                Ok(())
            }));
        }
    });

    let therad_checker = std::thread::spawn(move || {
        loop {
            let mut pool_guard = thread_pool.lock().unwrap();

            let mut to_remove = vec![];
            for (index, thread) in pool_guard.iter().enumerate() {
                if thread.is_finished() {
                    to_remove.push(index)
                }
            }

            for index in to_remove.into_iter().rev() {
                let thread = pool_guard.remove(index);
                thread.join().unwrap().unwrap();
            }

            drop(pool_guard);
            std::thread::sleep(Duration::from_millis(200));
        }
    });

    window.run()?;

    // idgaf
    let _ = therad_checker.join();

    Ok(())
}

struct Pipeline {
    source: Source,
    destination: Destination,
}

impl Pipeline {
    fn init<Input: AsRef<Path>, Output: AsRef<Path>>(
        input: Input,
        output: Output,
        rotate: Rotate,
    ) -> anyhow::Result<Self> {
        let source = Source::load(input)?;
        let destination = Destination::create(output, &source, &rotate)?;

        Ok(Self {
            source,
            destination,
        })
    }

    fn write_header(&mut self) -> anyhow::Result<()> {
        self.destination.write_header()?;
        Ok(())
    }

    fn configure(&mut self) -> anyhow::Result<()> {
        self.destination.setup_time_bases(&self.source)?;
        Ok(())
    }

    fn write_trailer(&mut self) -> anyhow::Result<()> {
        self.destination.write_trailer()?;
        Ok(())
    }

    fn pump_packets(&mut self) -> anyhow::Result<()> {
        for (input_stream, mut packet) in self.source.input_ctx.packets() {
            let istream_index: StreamId = input_stream.index().into();

            let in_time_base = self.source.time_bases[&istream_index];
            let out_time_base = self.destination.time_bases[&istream_index];

            match self.source.decoders.get_mut(&istream_index) {
                Some(decoder) => {
                    let encoder = self.destination.encoders.get_mut(&istream_index).ok_or(
                        anyhow::anyhow!("Found missing encoder to corresponding decoder"),
                    )?;
                    let filter =
                        self.destination
                            .filters
                            .get_mut(&istream_index)
                            .ok_or(anyhow::anyhow!(
                                "Found missing filter to corresponding decoder"
                            ))?;

                    packet.rescale_ts(input_stream.time_base(), in_time_base);

                    let mut pipe = Pipe {
                        output_ctx: &mut self.destination.output_ctx,
                        decoder,
                        filter,
                        encoder,
                        stream_id: istream_index,
                        in_time_base,
                        out_time_base,
                    };
                    pipe.decode_packet(&packet)?;
                    pipe.apply_filter()?;
                    pipe.encode_packets()?;
                }
                None => {
                    // Do stream copy on non-video streams.
                    packet.rescale_ts(in_time_base, out_time_base);
                    packet.set_position(-1);
                    packet.set_stream(istream_index.0);
                    packet
                        .write_interleaved(&mut self.destination.output_ctx)
                        .unwrap();
                }
            }
        }

        for (id, decoder) in &mut self.source.decoders {
            let in_time_base = self.source.time_bases[id];
            let out_time_base = self.destination.time_bases[id];

            let encoder = self
                .destination
                .encoders
                .get_mut(id)
                .ok_or(anyhow::anyhow!(
                    "Found missing encoder to corresponding decoder"
                ))?;
            let filter = self.destination.filters.get_mut(id).ok_or(anyhow::anyhow!(
                "Found missing filter to corresponding decoder"
            ))?;

            let mut pipe = Pipe {
                output_ctx: &mut self.destination.output_ctx,
                decoder,
                filter,
                encoder,
                stream_id: *id,
                in_time_base,
                out_time_base,
            };

            pipe.send_eof_decoder()?;
            pipe.apply_filter()?;
            pipe.encode_packets()?;

            pipe.send_eof_encoder()?;
            pipe.apply_filter()?;
            pipe.encode_packets()?;
        }

        Ok(())
    }
}

struct Pipe<'a> {
    output_ctx: &'a mut ffmpeg::format::context::Output,
    decoder: &'a mut VideoDecoder,
    filter: &'a mut Filter,
    encoder: &'a mut VideoEncoder,
    stream_id: StreamId,
    in_time_base: ffmpeg::Rational,
    out_time_base: ffmpeg::Rational,
}

impl<'a> Pipe<'a> {
    fn send_eof_decoder(&mut self) -> anyhow::Result<()> {
        self.decoder.send_eof()?;
        self.decoder.process_frames(|frame| {
            let timestamp = frame.timestamp();
            frame.set_pts(timestamp);

            self.filter.send_frame(frame)
        })
    }

    fn send_eof_encoder(&mut self) -> Result<(), ffmpeg::Error> {
        self.encoder.send_eof()
    }

    fn decode_packet(&mut self, packet: &ffmpeg::Packet) -> anyhow::Result<()> {
        self.decoder.send_packet(packet)?;
        self.decoder.process_frames(|frame| {
            let timestamp = frame.timestamp();
            frame.set_pts(timestamp);

            self.filter.send_frame(frame)
        })
    }

    fn apply_filter(&mut self) -> anyhow::Result<()> {
        self.filter.process_frames(|frame| {
            self.encoder.send_frame(frame)?;
            Ok(())
        })
    }

    fn encode_packets(&mut self) -> Result<(), ffmpeg::Error> {
        self.encoder.process_packets(|packet| {
            packet.set_stream(self.stream_id.0);
            packet.rescale_ts(self.in_time_base, self.out_time_base);
            packet.write_interleaved(self.output_ctx)
        })
    }
}

struct Source {
    _input_file: PathBuf,
    input_ctx: ffmpeg::format::context::Input,
    decoders: HashMap<StreamId, VideoDecoder>,
    time_bases: HashMap<StreamId, ffmpeg::Rational>,
}

impl Source {
    fn load<Input: AsRef<Path>>(input: Input) -> anyhow::Result<Self> {
        let input_ctx = ffmpeg::format::input(input.as_ref())?;

        ffmpeg::format::context::input::dump(&input_ctx, 0, input.as_ref().to_str());

        let mut decoders = HashMap::new();
        let mut time_bases = HashMap::new();
        for (index, stream) in input_ctx.streams().enumerate() {
            let media_type = stream.parameters().medium();
            time_bases.insert(index.into(), stream.time_base());

            if let ffmpeg::media::Type::Video = media_type {
                let decoder_context = ffmpeg::codec::Context::from_parameters(stream.parameters())?;
                let mut decoder = decoder_context.decoder().video()?;
                decoder.set_parameters(stream.parameters())?;

                decoders.insert(index.into(), decoder.into());
            }
        }

        Ok(Self {
            _input_file: input.as_ref().to_owned(),
            input_ctx,
            decoders,
            time_bases,
        })
    }
}

struct Destination {
    output_file: PathBuf,
    output_ctx: ffmpeg::format::context::Output,
    filters: HashMap<StreamId, Filter>,
    encoders: HashMap<StreamId, VideoEncoder>,
    time_bases: HashMap<StreamId, ffmpeg::Rational>,
}

impl Destination {
    fn create<Output: AsRef<Path>>(
        output: Output,
        source: &Source,
        rotate: &Rotate,
    ) -> anyhow::Result<Self> {
        let mut output_ctx = ffmpeg::format::output(output.as_ref())?;
        output_ctx.set_metadata(source.input_ctx.metadata().to_owned());

        let mut filters = HashMap::new();
        let mut encoders = HashMap::new();

        for (index, input_stream) in source.input_ctx.streams().enumerate() {
            if let Some(decoder) = source.decoders.get(&index.into()) {
                let encoder = VideoEncoder::create_from_decoder(decoder, &input_stream, rotate)?;
                let codec = encoder.codec().ok_or(anyhow::anyhow!(
                    "Unknown codec. The encoder was wrongly configured."
                ))?;
                let mut output_stream = output_ctx.add_stream(codec)?;
                output_stream.set_parameters(&encoder);

                filters.insert(index.into(), Filter::create(decoder, rotate)?);
                encoders.insert(index.into(), encoder);
            } else {
                // Set up for stream copy for non-video stream.
                let mut output_stream =
                    output_ctx.add_stream(ffmpeg::encoder::find(ffmpeg::codec::Id::None))?;
                output_stream.set_parameters(input_stream.parameters());
                // We need to set codec_tag to 0 lest we run into incompatible codec tag
                // issues when muxing into a different container format. Unfortunately
                // there's no high level API to do this (yet).
                unsafe {
                    (*output_stream.parameters().as_mut_ptr()).codec_tag = 0;
                }
            }
        }

        Ok(Self {
            output_file: output.as_ref().to_owned(),
            output_ctx,
            filters,
            encoders,
            // INFO: it's unknown until begin of writing into a file
            time_bases: HashMap::new(),
        })
    }

    fn write_header(&mut self) -> anyhow::Result<()> {
        ffmpeg::format::context::output::dump(&self.output_ctx, 0, self.output_file.to_str());
        self.output_ctx.write_header()?;
        Ok(())
    }

    fn write_trailer(&mut self) -> anyhow::Result<()> {
        self.output_ctx.write_trailer()?;
        Ok(())
    }

    fn setup_time_bases(&mut self, source: &Source) -> anyhow::Result<()> {
        for (index, _) in source.input_ctx.streams().enumerate() {
            let output_stream = self
                .output_ctx
                .stream(index)
                .ok_or(anyhow::anyhow!("Found missing stream in destination."))?;
            self.time_bases
                .insert(index.into(), output_stream.time_base());
        }

        Ok(())
    }
}

struct Filter {
    filter_graph: ffmpeg::filter::Graph,
}

impl Filter {
    fn create(decoder: &VideoDecoder, rotate: &Rotate) -> Result<Self, ffmpeg::Error> {
        let mut filter_graph = ffmpeg::filter::Graph::new();

        let mut filter_args = format!(
            "video_size={}x{}:pixel_aspect=1/1",
            decoder.width(),
            decoder.height(),
        );
        if let Some(pix_fmt) = decoder.format().descriptor() {
            filter_args = filter_args + ":pix_fmt=" + pix_fmt.name();
        }

        if let Some(frame_rate) = decoder.frame_rate() {
            filter_args = filter_args
                + ":time_base="
                + &frame_rate.denominator().to_string()
                + "/"
                + &frame_rate.numerator().to_string();
        }

        if let Some(color_space) = decoder.color_space().name() {
            filter_args = filter_args + ":colorspace=" + color_space;
        }

        if let Some(color_range) = decoder.color_range().name() {
            filter_args = filter_args + ":range=" + color_range;
        }

        dbg!(&filter_args);

        filter_graph.add(&ffmpeg::filter::find("buffer").unwrap(), "in", &filter_args)?;
        filter_graph.add(&ffmpeg::filter::find("buffersink").unwrap(), "out", "")?;

        filter_graph
            .output("in", 0)?
            .input("out", 0)?
            .parse(rotate.as_filter())?;
        filter_graph.validate()?;

        Ok(Self { filter_graph })
    }

    fn send_frame(&mut self, frame: &ffmpeg::frame::Video) -> anyhow::Result<()> {
        self.filter_graph
            .get("in")
            .ok_or(anyhow::anyhow!("Found missing input of filter graph!"))?
            .source()
            .add(frame)?;
        Ok(())
    }

    fn process_frames<F: FnMut(&mut ffmpeg::frame::Video) -> anyhow::Result<()>>(
        &mut self,
        mut processor: F,
    ) -> anyhow::Result<()> {
        let mut frame = ffmpeg::frame::Video::empty();

        while self
            .filter_graph
            .get("out")
            .ok_or(anyhow::anyhow!("Found missing output for a filter!"))?
            .sink()
            .frame(&mut frame)
            .is_ok()
        {
            processor(&mut frame)?;
        }

        Ok(())
    }
}

#[allow(unused)]
enum Rotate {
    Deg0,
    Deg90,
    Deg180,
    Deg270,
}

impl Rotate {
    fn as_filter(&self) -> &str {
        match self {
            Rotate::Deg0 => "null",
            Rotate::Deg90 => "transpose=1",
            Rotate::Deg180 => "transpose=2,transpose=2",
            Rotate::Deg270 => "transpose=2",
        }
    }

    fn is_axis_flips(&self) -> bool {
        match self {
            Rotate::Deg0 | Rotate::Deg180 => false,
            Rotate::Deg90 | Rotate::Deg270 => true,
        }
    }
}

impl From<RotationValue> for Rotate {
    fn from(value: RotationValue) -> Self {
        match value {
            RotationValue::NoRotation => Rotate::Deg0,
            RotationValue::Deg90 => Rotate::Deg90,
            RotationValue::Deg180 => Rotate::Deg180,
            RotationValue::Deg270 => Rotate::Deg270,
        }
    }
}

macro_rules! impl_from {
    ($origin:ty => $dest:ident) => {
        impl From<$origin> for $dest {
            fn from(value: $origin) -> $dest {
                $dest(value)
            }
        }
    };

    ($origin:ty => $dest:ident::$variant:ident) => {
        impl From<$origin> for $dest {
            fn from(value: $origin) -> $dest {
                $dest::$variant(value)
            }
        }
    };
}

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
struct StreamId(usize);

impl_from!(usize => StreamId);

struct VideoDecoder(ffmpeg::codec::decoder::Video);

impl VideoDecoder {
    fn process_frames<F: FnMut(&mut ffmpeg::frame::Video) -> anyhow::Result<()>>(
        &mut self,
        mut processor: F,
    ) -> anyhow::Result<()> {
        let mut frame = ffmpeg::frame::Video::empty();
        while self.0.receive_frame(&mut frame).is_ok() {
            processor(&mut frame)?;
        }

        Ok(())
    }
}

impl_from!(ffmpeg::codec::decoder::Video => VideoDecoder);

impl std::ops::Deref for VideoDecoder {
    type Target = ffmpeg::codec::decoder::Video;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for VideoDecoder {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

struct VideoEncoder(ffmpeg::codec::encoder::Video);

impl VideoEncoder {
    fn create_from_decoder(
        decoder: &VideoDecoder,
        corresponding_stream: &ffmpeg::Stream,
        rotate: &Rotate,
    ) -> anyhow::Result<Self> {
        let video = &decoder.0;

        // TODO: maybe need to handle different codecs.
        let output_codec = ffmpeg::encoder::find(ffmpeg::codec::Id::H264)
            .ok_or(anyhow::anyhow!("Cannot make encoder from given codec"))?;

        let encoder_context = ffmpeg::codec::Context::new_with_codec(output_codec);
        let mut encoder = encoder_context.encoder().video()?;

        let (mut width, mut height) = (video.width(), video.height());

        if rotate.is_axis_flips() {
            (width, height) = (height, width);
        }

        encoder.set_width(width);
        encoder.set_height(height);
        encoder.set_format(video.format());
        encoder.set_frame_rate(video.frame_rate());
        encoder.set_time_base(corresponding_stream.time_base());

        let mut options = ffmpeg::Dictionary::new();
        options.set("preset", "medium");
        options.set("crf", "23");

        Ok(encoder.open_with(options)?.into())
    }

    fn process_packets<F: FnMut(&mut ffmpeg::Packet) -> Result<(), ffmpeg::Error>>(
        &mut self,
        mut processor: F,
    ) -> Result<(), ffmpeg::Error> {
        let mut packet = ffmpeg::Packet::empty();
        while self.0.receive_packet(&mut packet).is_ok() {
            processor(&mut packet)?;
        }

        Ok(())
    }
}

impl_from!(ffmpeg::codec::encoder::Video => VideoEncoder);

impl std::ops::Deref for VideoEncoder {
    type Target = ffmpeg::codec::encoder::Video;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for VideoEncoder {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<&VideoEncoder> for ffmpeg::codec::Parameters {
    fn from(value: &VideoEncoder) -> Self {
        (&value.0).into()
    }
}
