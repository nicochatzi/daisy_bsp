use stm32h7xx_hal_dma as hal;
use hal::gpio;
use hal::time;
use hal::dma;
use hal::sai::{ self, SaiI2sExt, SaiChannel };

use hal::hal as embedded_hal;
use embedded_hal::digital::v2::OutputPin;

use hal::pac;
use pac::interrupt;

use alloc::prelude::v1::Box;


// = global constants =========================================================

pub const BLOCK_LENGTH: usize = 32;                             // 32 samples
pub const HALF_DMA_BUFFER_LENGTH: usize = BLOCK_LENGTH * 2;     //  2 channels
pub const DMA_BUFFER_LENGTH:usize = HALF_DMA_BUFFER_LENGTH * 2; //  2 half-blocks

pub const FS: time::Hertz = time::Hertz(48_000);


// = types ====================================================================

pub type Frame = (f32, f32);
pub type Block = [Frame; BLOCK_LENGTH];

type Sai1Pins = (
    gpio::gpiob::PB11<gpio::Output<gpio::PushPull>>,  // PDN
    gpio::gpioe::PE2<gpio::Alternate<gpio::AF6>>,     // MCLK_A
    gpio::gpioe::PE5<gpio::Alternate<gpio::AF6>>,     // SCK_A
    gpio::gpioe::PE4<gpio::Alternate<gpio::AF6>>,     // FS_A
    gpio::gpioe::PE6<gpio::Alternate<gpio::AF6>>,     // SD_A
    gpio::gpioe::PE3<gpio::Alternate<gpio::AF6>>,     // SD_B
);

type TransferDma1Str0 = dma::Transfer<dma::dma::Stream0<pac::DMA1>,
                                      pac::SAI1,
                                      dma::MemoryToPeripheral,
                                      &'static mut [u32; DMA_BUFFER_LENGTH]>;

type TransferDma1Str1 = dma::Transfer<dma::dma::Stream1<pac::DMA1>,
                                      pac::SAI1,
                                      dma::PeripheralToMemory,
                                      &'static mut [u32; DMA_BUFFER_LENGTH]>;

#[repr(C)]
pub struct OpaqueInterface { _private: [u8; 0] }


// = static data ==============================================================

#[link_section = ".sram1_bss"]
static mut TX_BUFFER: [u32; DMA_BUFFER_LENGTH] = [0; DMA_BUFFER_LENGTH];
#[link_section = ".sram1_bss"]
static mut RX_BUFFER: [u32; DMA_BUFFER_LENGTH] = [0; DMA_BUFFER_LENGTH];


// = audio::Interface =========================================================

type Error = u32;

pub struct Interface<'a> {
    pub fs: time::Hertz,
    closure: Option<Box<dyn FnMut(f32, &mut Block) + 'a>>,
    ak4556_reset: Option<gpio::gpiob::PB11<gpio::Output<gpio::PushPull>>>,
    hal_dma1_stream0: Option<TransferDma1Str0>,
    hal_dma1_stream1: Option<TransferDma1Str1>,
    hal_sai1: Option<hal::sai::Sai<pac::SAI1, hal::sai::I2S>>,
    _marker: core::marker::PhantomData<&'a *const ()>,
}


impl<'a> Interface<'a>
{
    pub fn new() -> Interface<'a> {
        Self {
            fs: FS,
            closure: None,
            ak4556_reset: None,
            hal_dma1_stream0: None,
            hal_dma1_stream1: None,
            hal_sai1: None,
            _marker: core::marker::PhantomData,
        }
    }

    pub fn init(
        clocks: &hal::rcc::CoreClocks,
        rec: hal::rcc::rec::Sai1, // reset and enable control
        pins: Sai1Pins,
        dma1_hal: hal::rcc::rec::Dma1
    ) -> Result<Interface<'a>, Error> {

        // - configure dma1 ---------------------------------------------------

        let dma1_streams = dma::dma::StreamsTuple::new(unsafe { pac::Peripherals::steal().DMA1 }, dma1_hal);

        // dma1 stream 0
        let tx_buffer: &'static mut [u32; DMA_BUFFER_LENGTH] = unsafe { &mut TX_BUFFER };
        let dma_config = dma::dma::DmaConfig::default()
            .priority(dma::config::Priority::High)
            .memory_increment(true)
            .peripheral_increment(false)
            .circular_buffer(true)
            .fifo_enable(false);
        let dma1_str0: dma::Transfer<_, _, dma::MemoryToPeripheral, _> = dma::Transfer::init(
            dma1_streams.0,
            unsafe { pac::Peripherals::steal().SAI1 },
            tx_buffer,
            None,
            dma_config,
        );

        // dma1 stream 1
        let rx_buffer: &'static mut [u32; DMA_BUFFER_LENGTH] = unsafe { &mut RX_BUFFER };
        let dma_config = dma_config.transfer_complete_interrupt(true)
                                   .half_transfer_interrupt(true);
        let dma1_str1: dma::Transfer<_, _, dma::PeripheralToMemory, _> = dma::Transfer::init(
            dma1_streams.1,
            unsafe { pac::Peripherals::steal().SAI1 },
            rx_buffer,
            None,
            dma_config,
        );

        // - configure sai1 ---------------------------------------------------

        let sai1_tx_config = sai::I2SChanConfig::new(sai::I2SDir::Tx)
            .set_frame_sync_active_high(true)
            .set_clock_strobe(sai::I2SClockStrobe::Falling);

        let sai1_rx_config = sai::I2SChanConfig::new(sai::I2SDir::Rx)
            .set_sync_type(sai::I2SSync::Internal)
            .set_frame_sync_active_high(true)
            .set_clock_strobe(sai::I2SClockStrobe::Rising);

        let sai1_pins = (
            pins.1,
            pins.2,
            pins.3,
            pins.4,
            Some(pins.5),
        );

        let sai1 = unsafe { pac::Peripherals::steal().SAI1 }.i2s_ch_a(
            sai1_pins,
            FS,
            sai::I2SDataSize::BITS_24,
            rec,
            clocks,
            sai1_tx_config,
            Some(sai1_rx_config),
        );

        Ok(Self {
            fs: FS,
            closure: None,
            ak4556_reset: Some(pins.0),
            hal_dma1_stream0: Some(dma1_str0),
            hal_dma1_stream1: Some(dma1_str1),
            hal_sai1: Some(sai1),
            _marker: core::marker::PhantomData,
        })
    }


    pub fn start<F: FnMut(f32, &mut Block) + 'a>(&mut self, closure: F) -> Result<&mut Self, Error> {
        self.closure = Some(Box::new(closure));

        // TODO implement drop for Interface so we can set INTERFACE_PTR to None
        let opaque_interface_ptr: *const OpaqueInterface = unsafe {
            core::mem::transmute::<*const Interface,
                                   *const OpaqueInterface>(self)
        };
        unsafe { INTERFACE_PTR = Some(opaque_interface_ptr); }

        self.start_audio()?;

        Ok(self) // TODO TypeState for a started interface
    }


    fn start_audio(&mut self) -> Result<(), Error> {

        // - AK4556 -----------------------------------------------------------

        let ak4556_reset = self.ak4556_reset.as_mut().unwrap();
        ak4556_reset.set_low().unwrap();
        use cortex_m::asm;
        asm::delay(480_000);     // ~ 1ms (datasheet specifies minimum 150ns)
        ak4556_reset.set_high().unwrap();


        // - start audio ------------------------------------------------------

        // unmask interrupt handler for dma 1, stream 1
        unsafe { pac::NVIC::unmask(pac::Interrupt::DMA1_STR1); }

        let dma1_str0 = self.hal_dma1_stream0.as_mut().unwrap();
        let dma1_str1 = self.hal_dma1_stream1.as_mut().unwrap();
        let sai1 = self.hal_sai1.as_mut().unwrap();

        dma1_str1.start(|_sai1_rb| {
            sai1.enable_dma(SaiChannel::ChannelB);
        });

        dma1_str0.start(|sai1_rb| {
            sai1.enable_dma(SaiChannel::ChannelA);

            // wait until sai1's fifo starts to receive data
            while sai1_rb.cha.sr.read().flvl().is_empty() { }

            sai1.enable();
        });

        Ok(())
    }
}


// = dma rx interrupt handler =================================================

static mut INTERFACE_PTR: Option<*const OpaqueInterface> = None;

#[interrupt]
fn DMA1_STR1() {
    if unsafe { INTERFACE_PTR.is_none() } {
        return;
    }

    // reconstitute mutable reference to Interface
    let interface_ptr: *const OpaqueInterface = unsafe { INTERFACE_PTR }.unwrap();
    let interface_ptr: *mut Interface = unsafe {
        core::mem::transmute::<*const OpaqueInterface,
                               *mut Interface>(interface_ptr)
    };
    let interface: &mut Interface = unsafe { &mut *interface_ptr };

    let transfer = interface.hal_dma1_stream1.as_mut().unwrap();

    let skip = if transfer.get_half_transfer_flag() {
        transfer.clear_half_transfer_interrupt();
        (0, HALF_DMA_BUFFER_LENGTH)

    } else if transfer.get_transfer_complete_flag() {
        transfer.clear_transfer_complete_interrupt();
        (HALF_DMA_BUFFER_LENGTH, 0)

    } else {
        // TODO handle error flags once HAL supports them
        return;
    };

    dma_common(skip);
}


#[inline(always)]
fn dma_common(skip: (usize, usize)) {
    // convert audio data from u32 to f32
    use core::num::Wrapping;
    #[inline(always)]
    fn u32_to_f32(y: u32) -> f32 {
        let y = (Wrapping(y) + Wrapping(0x0080_0000)).0 & 0x00FF_FFFF; // convert to i32
        let y = (y as f32 / 8_388_608.) - 1.;  // (2^24) / 2
        y
    }

    // convert audio data from f32 to u24
    #[inline(always)]
    pub fn f32_to_u24(x: f32) -> u32 {
        //return (int16_t) __SSAT((int32_t) (x * 32767.f), 16);
        let x = x * 8_388_607.;
        let x = if x > 8_388_607. {
            8_388_607.
        } else if x < -8_388_608. {
            -8_388_608.
        } else {
            x
        };
        (x as i32) as u32
    }

    // callback buffer
    static mut BLOCK: Block = [(0., 0.); BLOCK_LENGTH];

    // convert & copy rx buffer to callback buffer
    let mut dma_index: usize = 0;
    let mut block_index: usize = 0;
    while dma_index < HALF_DMA_BUFFER_LENGTH {
        let rx0: usize = dma_index + skip.1;
        let rx1: usize = rx0 + 1;

        let rx_y0 = unsafe { RX_BUFFER[rx0] };
        let rx_y1 = unsafe { RX_BUFFER[rx1] };

        let y0 = u32_to_f32(rx_y0);
        let y1 = u32_to_f32(rx_y1);
        unsafe { BLOCK[block_index] = (y1, y0); }

        dma_index += 2;
        block_index += 1;
    }

    // invoke closure
    if let Some(interface_ptr) = unsafe { INTERFACE_PTR }  {
        let interface_ptr: *mut Interface = unsafe {
            core::mem::transmute::<*const OpaqueInterface,
                                   *mut Interface>(interface_ptr)
        };
        if let Some(closure) = unsafe { &mut (*interface_ptr).closure } {
            let fs = unsafe { (*interface_ptr).fs };
            closure(fs.0 as f32, unsafe { &mut BLOCK });
        }
    }

    // convert & copy callback buffer to tx buffer
    let mut dma_index: usize = 0;
    let mut block_index: usize = 0;
    while dma_index < HALF_DMA_BUFFER_LENGTH {
        let tx0: usize = dma_index + skip.0;
        let tx1: usize = tx0 + 1;

        let (y0, y1) = unsafe { BLOCK[block_index] };
        unsafe { TX_BUFFER[tx1] = f32_to_u24(y0) };
        unsafe { TX_BUFFER[tx0] = f32_to_u24(y1) };

        dma_index += 2;
        block_index += 1;
    }
}
