use kzg::{FFTFr, FFTSettings, Fr, FFTG1, G1};

pub fn roundtrip_fft<TFr: Fr, TG1: G1, TFFTSettings: FFTSettings<TFr> + FFTG1<TG1>>(
    make_data: &dyn Fn(usize) -> Vec<TG1>,
) {
    let size: usize = 10;
    let mut fs = TFFTSettings::new(size).unwrap();
    assert_eq!(fs.get_max_width(), 2 << size - 1);

    // Make data
    let expected = make_data(fs.get_max_width());
    let mut data = make_data(fs.get_max_width());

    // Forward and reverse FFT
    let coeffs = fs.fft_g1(&data, false).unwrap();
    assert_eq!(coeffs.len(), 2 << size - 1);
    data = fs.fft_g1(&coeffs, true).unwrap();
    assert_eq!(data.len(), 2 << size - 1);

    // Verify that the result is still ascending values of i
    for i in 0..fs.get_max_width() {
        assert!(expected[i].equals(&data[i]));
    }

    fs.destroy();
}

pub fn stride_fft<TFr: Fr, TG1: G1, TFFTSettings: FFTSettings<TFr> + FFTG1<TG1>>(
    make_data: &dyn Fn(usize) -> Vec<TG1>,
) {
    let size1: usize = 9;
    let size2: usize = 12;
    let width: u64 = if size1 < size2 {
        1 << size1
    } else {
        1 << size2
    };

    let mut fs1 = TFFTSettings::new(size1).unwrap();
    assert_eq!(fs1.get_max_width(), 2 << size1 - 1);
    let mut fs2 = TFFTSettings::new(size2).unwrap();
    assert_eq!(fs2.get_max_width(), 2 << size2 - 1);

    let data = make_data(width as usize);

    let coeffs1 = fs1.fft_g1(&data, false).unwrap();
    assert_eq!(coeffs1.len(), width as usize);
    let coeffs2 = fs2.fft_g1(&data, false).unwrap();
    assert_eq!(coeffs2.len(), width as usize);

    for i in 0..width {
        assert!(coeffs1[i as usize].equals(&coeffs2[i as usize]));
    }

    fs1.destroy();
    fs2.destroy();
}

pub fn compare_sft_fft<TFr: Fr, TG1: G1, TFFTSettings: FFTSettings<TFr> + FFTFr<TFr>>(
    fft_g1_slow: &dyn Fn(&mut [TG1], &[TG1], usize, &[TFr], usize, usize),
    fft_g1_fast: &dyn Fn(&mut [TG1], &[TG1], usize, &[TFr], usize, usize),
    make_data: &dyn Fn(usize) -> Vec<TG1>
) {
    let size: usize = 6;
    let mut fft_settings = TFFTSettings::new(size).unwrap();
    let mut slow = vec![TG1::default(); fft_settings.get_max_width()];
    let mut fast = vec![TG1::default(); fft_settings.get_max_width()];
    let data = make_data(fft_settings.get_max_width());

    fft_g1_slow(&mut slow, &data, 1, fft_settings.get_expanded_roots_of_unity(), 1, fft_settings.get_max_width());
    fft_g1_fast(&mut fast, &data, 1, fft_settings.get_expanded_roots_of_unity(), 1, fft_settings.get_max_width());

    for i in 0..fft_settings.get_max_width() {
        assert!(slow[i].equals(&fast[i]));
    }

    fft_settings.destroy();
}
