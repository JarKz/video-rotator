use ffmpeg_next as ffmpeg;

fn main() -> anyhow::Result<()> {
    ffmpeg::init()?;

    const INPUT_FILE: &str = "input.mp4";
    const OUTPUT_FILE: &str = "output.mp4";

    let mut ictx = ffmpeg::format::input(INPUT_FILE)?;
    let mut octx = ffmpeg::format::output(OUTPUT_FILE)?;

    ffmpeg::format::context::input::dump(&ictx, 0, INPUT_FILE.into());

    let istream = ictx
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or(anyhow::anyhow!("No best stream for input video"))?;
    let input_video_index = istream.index();

    let decoder_context = ffmpeg::codec::Context::from_parameters(istream.parameters())?;
    let mut decoder = decoder_context.decoder().video()?;
    decoder.set_parameters(istream.parameters())?;

    let output_codec = ffmpeg::encoder::find(ffmpeg::codec::Id::H264)
        .ok_or(anyhow::anyhow!("Cannot make encoder from given codec"))?;

    let mut ostream = octx.add_stream(output_codec)?;
    let output_video_index = ostream.index();
    let encoder_context = ffmpeg::codec::Context::new_with_codec(output_codec);
    let mut encoder = encoder_context.encoder().video()?;

    encoder.set_width(decoder.width());
    encoder.set_height(decoder.height());
    encoder.set_format(decoder.format());
    encoder.set_frame_rate(decoder.frame_rate());
    encoder.set_time_base(istream.time_base());

    let mut options = ffmpeg::Dictionary::new();
    options.set("preset", "medium");
    options.set("crf", "23");
    options.set("profile", "high");

    let mut opened_encoder = encoder.open_with(options)?;
    ostream.set_parameters(&opened_encoder);

    println!("input: {input_video_index}, output: {output_video_index}");

    octx.set_metadata(ictx.metadata().to_owned());
    ffmpeg::format::context::output::dump(&octx, 0, OUTPUT_FILE.into());
    octx.write_header()?;

    let in_time_base = istream.time_base();
    let out_time_base = octx.stream(0).unwrap().time_base();

    dbg!(in_time_base, out_time_base);

    let mut filter_graph = ffmpeg::filter::Graph::new();

    let filter_args = format!(
        "video_size={}x{}:pix_fmt={}:time_base={}/{}:pixel_aspect=1/1:colorspace={}:range={}",
        decoder.width(),
        decoder.height(),
        decoder.format().descriptor().unwrap().name(),
        decoder.frame_rate().unwrap().denominator(),
        decoder.frame_rate().unwrap().numerator(),
        decoder.color_space().name().unwrap(),
        decoder.color_range().name().unwrap(),
    );
    dbg!(&filter_args);

    filter_graph.add(&ffmpeg::filter::find("buffer").unwrap(), "in", &filter_args)?;
    filter_graph.add(&ffmpeg::filter::find("buffersink").unwrap(), "out", "")?;

    filter_graph
        .output("in", 0)?
        .input("out", 0)?
        .parse("transpose=2,transpose=2")?;
    filter_graph.validate()?;

    for (stream, mut packet) in ictx.packets() {
        if stream.index() == input_video_index {
            packet.rescale_ts(stream.time_base(), in_time_base);

            decoder.send_packet(&packet)?;

            let mut frame = ffmpeg::frame::Video::empty();
            while let Ok(()) = decoder.receive_frame(&mut frame) {
                let timestamp = frame.timestamp();
                frame.set_pts(timestamp);

                filter_graph.get("in").unwrap().source().add(&frame)?;

                let mut filtered_frame = ffmpeg::frame::Video::empty();
                while filter_graph
                    .get("out")
                    .unwrap()
                    .sink()
                    .frame(&mut filtered_frame)
                    .is_ok()
                {
                    filtered_frame.set_pts(timestamp);
                    opened_encoder.send_frame(&filtered_frame)?;

                    let mut out_packet = ffmpeg::Packet::empty();
                    while opened_encoder.receive_packet(&mut out_packet).is_ok() {
                        out_packet.set_stream(output_video_index);
                        out_packet.rescale_ts(in_time_base, out_time_base);
                        out_packet.write_interleaved(&mut octx)?;
                    }
                }
            }
        }
    }

    decoder.send_eof()?;
    opened_encoder.send_eof()?;

    let mut out_packet = ffmpeg::Packet::empty();
    while opened_encoder.receive_packet(&mut out_packet).is_ok() {
        out_packet.set_stream(output_video_index);
        out_packet.rescale_ts(in_time_base, out_time_base);
        out_packet.write_interleaved(&mut octx)?;
    }

    octx.write_trailer()?;

    Ok(())
}
